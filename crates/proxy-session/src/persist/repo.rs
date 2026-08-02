//! SessionRepo: persistence layer for the proxy-session tables.
//!
//! Opens its own SQLite connection (WAL + busy_timeout) against the same
//! database file used by proxy-store, and only touches its own tables.

use std::sync::Mutex;

use rusqlite::{params, Connection, OptionalExtension};

use crate::domain::model_call::ModelCallRow;
use crate::domain::tool_invocation::ToolInvocationRow;
use crate::ingest::observation::{Observation, ObservationKind, TokenUsage};
use crate::persist::schema;
use crate::SessionResult;

/// Configuration for opening a SessionRepo.
#[derive(Debug, Clone)]
pub struct SessionRepoConfig {
    pub database_path: std::path::PathBuf,
    pub busy_timeout_ms: u64,
}

impl Default for SessionRepoConfig {
    fn default() -> Self {
        Self {
            database_path: std::path::PathBuf::from("data/datav2.db"),
            busy_timeout_ms: 5000,
        }
    }
}

/// Repository over the proxy-session tables.
#[derive(Clone)]
pub struct SessionRepo {
    inner: std::sync::Arc<SessionRepoInner>,
}

struct SessionRepoInner {
    conn: Mutex<Connection>,
}

impl SessionRepo {
    /// Open the repository, creating its tables if missing.
    pub fn open(config: SessionRepoConfig) -> SessionResult<Self> {
        if let Some(parent) = config.database_path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| crate::SessionError::InvalidArgument(e.to_string()))?;
            }
        }
        let conn = open_connection(&config)?;
        schema::migrate(&conn)?;
        Ok(Self {
            inner: std::sync::Arc::new(SessionRepoInner {
                conn: Mutex::new(conn),
            }),
        })
    }

    /// Record an observation (idempotent by event_id).
    pub fn record_observation(&self, obs: &Observation) -> SessionResult<()> {
        let conn = self.inner.conn.lock().unwrap();
        self.insert_observation(&conn, obs)?;
        Ok(())
    }

    /// Materialize observations into model_calls / tool_invocations for a session.
    pub fn materialize(&self, session_id: &str) -> SessionResult<()> {
        let conn = self.inner.conn.lock().unwrap();
        let observations = self.load_observations(&conn, session_id)?;
        for obs in &observations {
            self.apply_observation(&conn, obs)?;
        }
        Ok(())
    }

    // ── Queries ──

    /// List model calls for a session, ordered by sequence (keyset pagination).
    pub fn list_model_calls(
        &self,
        session_id: &str,
        after_seq: Option<i64>,
        limit: i64,
    ) -> SessionResult<Vec<ModelCallRow>> {
        let conn = self.inner.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, session_id, sequence_no, previous_model_call_id,
                    client_request_id, provider_request_id, status,
                    requested_model, resolved_model, provider, upstream,
                    input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens,
                    cost_microusd, duration_ms, ttft_ms, stop_reason, http_status_code,
                    error_type, error_message
             FROM model_calls
             WHERE session_id = ?1 AND (?2 IS NULL OR sequence_no > ?2)
             ORDER BY sequence_no ASC LIMIT ?3",
        )?;
        let rows = stmt
            .query_map(params![session_id, after_seq, limit], |row| {
                ModelCallRow::from_row(row)
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// List tool invocations for a model call.
    pub fn list_tool_invocations(
        &self,
        call_id: &str,
    ) -> SessionResult<Vec<ToolInvocationRow>> {
        let conn = self.inner.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, model_call_id, tool_use_id, operation_seq, tool_name, status,
                    started_at, ended_at, duration_ms,
                    model_input_preview, effective_input_preview,
                    raw_result_preview, effective_result_preview
             FROM tool_invocations
             WHERE model_call_id = ?1
             ORDER BY operation_seq ASC",
        )?;
        let rows = stmt
            .query_map(params![call_id], |row| ToolInvocationRow::from_row(row))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Find the model call id for a given tool_use_id.
    pub fn model_call_for_tool(&self, tool_use_id: &str) -> SessionResult<Option<String>> {
        let conn = self.inner.conn.lock().unwrap();
        let call_id = conn
            .query_row(
                "SELECT model_call_id FROM tool_invocations WHERE tool_use_id = ?1",
                params![tool_use_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        Ok(call_id)
    }

    // ── Internals ──

    fn insert_observation(&self, conn: &Connection, obs: &Observation) -> SessionResult<()> {
        conn.execute(
            "INSERT OR IGNORE INTO observations (
                event_id, session_id, source, event_type,
                occurred_at, received_at, source_sequence, source_version,
                payload_hash, raw_payload, model_call_id, agent_id, prompt_id, tool_use_id
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![
                obs.event_id,
                obs.session_id,
                obs.source,
                obs.event_type(),
                obs.occurred_at,
                obs.received_at,
                obs.source_sequence,
                obs.source_version,
                obs.payload_hash,
                serde_json::to_string(&obs.kind).unwrap_or_default(),
                extract_field(&obs.kind, "call_id"),
                extract_field(&obs.kind, "agent_id"),
                extract_field(&obs.kind, "prompt_id"),
                extract_field(&obs.kind, "tool_use_id"),
            ],
        )?;
        Ok(())
    }

    fn load_observations(&self, conn: &Connection, session_id: &str) -> SessionResult<Vec<Observation>> {
        let mut stmt = conn.prepare(
            "SELECT event_id, session_id, source, event_type,
                    occurred_at, received_at, source_sequence, source_version,
                    payload_hash, raw_payload
             FROM observations
             WHERE session_id = ?1
             ORDER BY received_at ASC, event_id ASC",
        )?;
        let rows = stmt.query_map(params![session_id], |row| {
            let raw: String = row.get("raw_payload")?;
            let kind = serde_json::from_str::<ObservationKind>(&raw).unwrap_or_else(|_| {
                ObservationKind::ModelCallEnd {
                    call_id: String::new(),
                    status: "failed".into(),
                    tokens: TokenUsage::default(),
                    stop_reason: None,
                    cost_microusd: 0,
                    ended_at: 0,
                    provider_request_id: None,
                    error: Some("unparseable observation payload".into()),
                    http_status_code: None,
                }
            });
            Ok(Observation {
                event_id: row.get("event_id")?,
                session_id: row.get("session_id")?,
                source: row.get("source")?,
                occurred_at: row.get("occurred_at")?,
                received_at: row.get("received_at")?,
                source_sequence: row.get("source_sequence")?,
                source_version: row.get("source_version")?,
                payload_hash: row.get("payload_hash")?,
                kind,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Apply one observation to the materialized tables (idempotent).
    fn apply_observation(&self, conn: &Connection, obs: &Observation) -> SessionResult<()> {
        match &obs.kind {
            ObservationKind::ModelCallStart {
                call_id,
                client_request_id,
                requested_model,
                started_at,
            } => {
                conn.execute(
                    "INSERT OR IGNORE INTO model_calls (
                        id, session_id, sequence_no, client_request_id, requested_model, status
                     ) VALUES (?1, ?2, ?3, ?4, ?5, 'in_progress')",
                    params![call_id, obs.session_id, 0, client_request_id, requested_model],
                )?;
                let _ = started_at;
            }
            ObservationKind::ModelCallFirstToken { call_id, ttft_ms } => {
                conn.execute(
                    "UPDATE model_calls SET ttft_ms = ?2 WHERE id = ?1 AND ttft_ms IS NULL",
                    params![call_id, *ttft_ms as i64],
                )?;
            }
            ObservationKind::ToolEmitted {
                call_id,
                tool_use_id,
                tool_name,
                started_at,
            } => {
                let operation_seq: i64 = conn.query_row(
                    "SELECT COALESCE(MAX(operation_seq), -1) + 1 FROM tool_invocations
                     WHERE model_call_id = ?1",
                    params![call_id],
                    |row| row.get(0),
                )?;
                conn.execute(
                    "INSERT OR IGNORE INTO tool_invocations (
                        id, model_call_id, tool_use_id, operation_seq, tool_name,
                        status, started_at
                     ) VALUES (?1, ?2, ?3, ?4, ?5, 'emitted', ?6)",
                    params![
                        format!("tool-{tool_use_id}"),
                        call_id,
                        tool_use_id,
                        operation_seq,
                        tool_name,
                        started_at,
                    ],
                )?;
            }
            ObservationKind::ToolInputDelta { .. } => {
                // Accumulated in ToolInputComplete via the parsed payload.
            }
            ObservationKind::ToolInputComplete { tool_use_id, input_json } => {
                conn.execute(
                    "UPDATE tool_invocations
                     SET status = 'input_complete', effective_input_preview = ?2
                     WHERE tool_use_id = ?1",
                    params![tool_use_id, truncate(input_json, 1000)],
                )?;
            }
            ObservationKind::ToolResult {
                tool_use_id,
                raw_result_preview,
                effective_result_preview,
                status,
            } => {
                conn.execute(
                    "UPDATE tool_invocations
                     SET status = ?2,
                         raw_result_preview = ?3,
                         effective_result_preview = ?4,
                         ended_at = ?5
                     WHERE tool_use_id = ?1",
                    params![
                        tool_use_id,
                        status,
                        raw_result_preview.as_deref().map(|s| truncate(s, 500)),
                        effective_result_preview.as_deref().map(|s| truncate(s, 500)),
                        obs.occurred_at,
                    ],
                )?;
            }
            ObservationKind::ModelCallEnd {
                call_id,
                status,
                tokens,
                stop_reason,
                cost_microusd,
                ended_at,
                provider_request_id,
                error,
                http_status_code,
            } => {
                conn.execute(
                    "UPDATE model_calls SET
                        status = ?2,
                        input_tokens = ?3,
                        output_tokens = ?4,
                        cache_creation_tokens = ?5,
                        cache_read_tokens = ?6,
                        cost_microusd = ?7,
                        duration_ms = ?8,
                        stop_reason = ?9,
                        provider_request_id = ?10,
                        error_type = ?11,
                        error_message = ?12,
                        http_status_code = ?13
                     WHERE id = ?1",
                    params![
                        call_id,
                        status,
                        tokens.input_tokens as i64,
                        tokens.output_tokens as i64,
                        tokens.cache_creation_tokens as i64,
                        tokens.cache_read_tokens as i64,
                        cost_microusd,
                        ended_at.saturating_sub(0),
                        stop_reason,
                        provider_request_id,
                        if error.is_some() { Some("upstream_error") } else { None },
                        error,
                        http_status_code.map(|c| c as i64),
                    ],
                )?;
                let _ = ended_at;
            }
            ObservationKind::PromptSubmit { .. } | ObservationKind::AgentStart { .. } | ObservationKind::AgentStop { .. } => {
                // Interaction / agent run grouping is handled by the reconciler (P1).
            }
        }
        Ok(())
    }
}

impl crate::ingest::SessionIngest for SessionRepo {
    fn record(&self, obs: crate::ingest::Observation) -> SessionResult<()> {
        self.record_observation(&obs)
    }
}

fn open_connection(config: &SessionRepoConfig) -> SessionResult<Connection> {
    let conn = Connection::open(&config.database_path)?;
    conn.busy_timeout(std::time::Duration::from_millis(config.busy_timeout_ms))?;
    conn.execute_batch("PRAGMA journal_mode = WAL;")?;
    conn.execute_batch("PRAGMA foreign_keys = ON;")?;
    Ok(conn)
}

fn extract_field(kind: &ObservationKind, field: &str) -> Option<String> {
    match kind {
        ObservationKind::ModelCallStart { call_id, .. }
        | ObservationKind::ModelCallFirstToken { call_id, .. }
        | ObservationKind::ModelCallEnd { call_id, .. } if field == "call_id" => {
            Some(call_id.clone())
        }
        ObservationKind::ToolEmitted { tool_use_id, .. }
        | ObservationKind::ToolInputDelta { tool_use_id, .. }
        | ObservationKind::ToolInputComplete { tool_use_id, .. }
        | ObservationKind::ToolResult { tool_use_id, .. } if field == "tool_use_id" => {
            Some(tool_use_id.clone())
        }
        ObservationKind::PromptSubmit { prompt_id, .. } if field == "prompt_id" => {
            Some(prompt_id.clone())
        }
        ObservationKind::AgentStart { agent_id, .. }
        | ObservationKind::AgentStop { agent_id, .. } if field == "agent_id" => {
            Some(agent_id.clone())
        }
        _ => None,
    }
}

fn truncate(s: &str, max_chars: usize) -> String {
    let mut chars = s.chars();
    let preview: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{preview}…")
    } else {
        preview
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::observation::{Observation, ObservationKind, TokenUsage};

    fn repo() -> (SessionRepo, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session-test.db");
        let repo = SessionRepo::open(SessionRepoConfig {
            database_path: path.clone(),
            ..Default::default()
        })
        .unwrap();
        (repo, dir.into_path())
    }

    fn observation(kind: ObservationKind, session_id: &str, seq: usize) -> Observation {
        Observation {
            event_id: format!("ev-{seq}"),
            session_id: session_id.into(),
            source: "proxy".into(),
            occurred_at: 1_700_000_000_000 + seq as i64,
            received_at: 1_700_000_000_000 + seq as i64,
            source_sequence: None,
            source_version: None,
            payload_hash: format!("hash-{seq}"),
            kind,
        }
    }

    #[test]
    fn record_is_idempotent_by_event_id() {
        let (repo, dir) = repo();
        let obs = observation(
            ObservationKind::ModelCallStart {
                call_id: "call-1".into(),
                client_request_id: None,
                requested_model: Some("model".into()),
                started_at: 1_700_000_000_000,
            },
            "sess-1",
            1,
        );
        repo.record_observation(&obs).unwrap();
        repo.record_observation(&obs).unwrap();

        let conn = repo.inner.conn.lock().unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM observations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
        drop(conn);
        drop(repo);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn materialize_writes_model_call_and_tool() {
        let (repo, dir) = repo();
        repo.record_observation(&observation(
            ObservationKind::ModelCallStart {
                call_id: "call-1".into(),
                client_request_id: Some("req-1".into()),
                requested_model: Some("m1".into()),
                started_at: 1_700_000_000_000,
            },
            "sess-1",
            1,
        ))
        .unwrap();
        repo.record_observation(&observation(
            ObservationKind::ToolEmitted {
                call_id: "call-1".into(),
                tool_use_id: "tool-1".into(),
                tool_name: "Bash".into(),
                started_at: 1_700_000_000_100,
            },
            "sess-1",
            2,
        ))
        .unwrap();
        repo.record_observation(&observation(
            ObservationKind::ToolInputComplete {
                tool_use_id: "tool-1".into(),
                input_json: r#"{"command":"ls"}"#.into(),
            },
            "sess-1",
            3,
        ))
        .unwrap();
        repo.record_observation(&observation(
            ObservationKind::ModelCallEnd {
                call_id: "call-1".into(),
                status: "completed".into(),
                tokens: TokenUsage {
                    input_tokens: 10,
                    output_tokens: 5,
                    ..Default::default()
                },
                stop_reason: Some("tool_use".into()),
                cost_microusd: 42,
                ended_at: 1_700_000_000_200,
                provider_request_id: None,
                error: None,
                http_status_code: Some(200),
            },
            "sess-1",
            4,
        ))
        .unwrap();

        repo.materialize("sess-1").unwrap();

        let calls = repo.list_model_calls("sess-1", None, 100).unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].status.as_str(), "completed");
        assert_eq!(calls[0].cost_microusd, 42);

        let tools = repo.list_tool_invocations("call-1").unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].tool_name, "Bash");
        assert_eq!(tools[0].status.as_str(), "input_complete");
        assert_eq!(repo.model_call_for_tool("tool-1").unwrap(), Some("call-1".into()));
        drop(repo);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
