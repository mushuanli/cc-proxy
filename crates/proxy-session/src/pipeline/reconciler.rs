//! Reconciler: idempotently build Interaction/ExecutionRun/AgentRun groups
//! from observations. Deterministic and re-runnable.

use std::collections::HashMap;
use std::sync::Arc;

use rusqlite::{params, Connection, OptionalExtension};

use crate::domain::execution_run::RunKind;
use crate::ingest::observation::Observation;
use crate::persist::repo::SessionRepo;
use crate::source::heuristic::HeuristicClassifier;
use crate::SessionResult;

/// Configuration for the reconciler.
#[derive(Debug, Clone)]
pub struct ReconcilerConfig {
    /// Whether to derive run grouping from request-body heuristics
    /// (enabled by default; exact hook/OTel relations always take precedence).
    pub enable_heuristics: bool,
}

impl Default for ReconcilerConfig {
    fn default() -> Self {
        Self {
            enable_heuristics: true,
        }
    }
}

/// Reconciles observations into the domain tables for a session.
pub struct Reconciler {
    repo: Arc<SessionRepo>,
    config: ReconcilerConfig,
}

impl Reconciler {
    pub fn new(repo: Arc<SessionRepo>, config: ReconcilerConfig) -> Self {
        Self { repo, config }
    }

    /// Reconcile all observations for a session. Idempotent.
    pub fn reconcile(&self, session_id: &str) -> SessionResult<()> {
        let observations = self.repo.load_observations_for(session_id)?;
        self.repo.with_conn(|conn| {
            let tx = conn.unchecked_transaction()?;
            self.apply_prompt_submits(&tx, &observations)?;
            self.apply_agent_starts(&tx, session_id, &observations)?;
            self.apply_model_call_grouping(&tx, session_id, &observations)?;
            self.apply_agent_parent_links(&tx, session_id, &observations)?;
            tx.commit()?;
            Ok(())
        })
    }

    // ── PromptSubmit → Interaction ──

    fn apply_prompt_submits(
        &self,
        conn: &Connection,
        observations: &[Observation],
    ) -> SessionResult<()> {
        for obs in observations {
            let crate::ingest::ObservationKind::PromptSubmit {
                prompt_id,
                prompt_text,
                started_at,
            } = &obs.kind
            else {
                continue;
            };
            let id = format!("inter-{}", &obs.session_id);
            conn.execute(
                "INSERT OR IGNORE INTO interactions (
                    id, session_id, external_prompt_id, prompt_text, started_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![id, obs.session_id, prompt_id, prompt_text, started_at],
            )?;
        }
        Ok(())
    }

    // ── AgentStart/AgentStop → AgentIdentity + AgentRun ──

    fn apply_agent_starts(
        &self,
        conn: &Connection,
        session_id: &str,
        observations: &[Observation],
    ) -> SessionResult<()> {
        // Track end times by agent_id for run closure.
        let mut ends: HashMap<&str, i64> = HashMap::new();
        let mut starts: Vec<(&str, &str, i64)> = Vec::new(); // (agent_id, agent_type, started_at)
        for obs in observations {
            match &obs.kind {
                crate::ingest::ObservationKind::AgentStart {
                    agent_id,
                    agent_type,
                    started_at,
                } => starts.push((agent_id, agent_type, *started_at)),
                crate::ingest::ObservationKind::AgentStop { agent_id, ended_at } => {
                    ends.insert(agent_id, *ended_at);
                }
                _ => {}
            }
        }

        let mut run_no_by_identity: HashMap<String, i64> = HashMap::new();
        for (agent_id, agent_type, started_at) in starts {
            // Ensure a synthetic main identity exists first.
            self.ensure_main_identity(conn, session_id)?;
            let identity_id = format!("ident-{}-{agent_id}", session_id);
            conn.execute(
                "INSERT OR IGNORE INTO agent_identities (
                    id, session_id, external_agent_id, agent_type, synthetic
                 ) VALUES (?1, ?2, ?3, ?4, 0)",
                params![identity_id, session_id, agent_id, agent_type],
            )?;
            let run_no = *run_no_by_identity
                .entry(identity_id.clone())
                .and_modify(|n| *n += 1)
                .or_insert(1);
            let run_id = format!("arun-{identity_id}-{run_no}");
            let status = if ends.contains_key(agent_id) { "completed" } else { "in_progress" };
            let ended_at = ends.get(agent_id).copied();
            conn.execute(
                "INSERT OR IGNORE INTO agent_runs (
                    id, session_id, identity_id, run_no, started_at, ended_at, status
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![run_id, session_id, identity_id, run_no, started_at, ended_at, status],
            )?;
        }
        Ok(())
    }

    fn ensure_main_identity(&self, conn: &Connection, session_id: &str) -> SessionResult<()> {
        let id = format!("ident-{session_id}-main");
        conn.execute(
            "INSERT OR IGNORE INTO agent_identities (
                id, session_id, external_agent_id, agent_type, synthetic
             ) VALUES (?1, ?2, NULL, 'main', 1)",
            params![id, session_id],
        )?;
        Ok(())
    }

    // ── ModelCall → Interaction + ExecutionRun grouping ──

    fn apply_model_call_grouping(
        &self,
        conn: &Connection,
        session_id: &str,
        observations: &[Observation],
    ) -> SessionResult<()> {
        // Build call_id → prompt facts from observations.
        let mut call_prompt: HashMap<String, String> = HashMap::new();
        for obs in observations {
            if let crate::ingest::ObservationKind::ModelCallStart {
                call_id, prompt_text, ..
            } = &obs.kind
            {
                if let Some(text) = prompt_text {
                    call_prompt.insert(call_id.clone(), text.clone());
                }
            }
        }

        // Fetch all model calls for the session (on the held connection).
        let calls = crate::persist::repo::SessionRepo::query_model_calls(
            conn,
            session_id,
            None,
            10_000,
        )?;
        for call in &calls {
            let (run_kind, prompt_text) = if self.config.enable_heuristics {
                let text = call_prompt.get(&call.id).map(String::as_str).unwrap_or("");
                let (kind, _) = HeuristicClassifier::classify(text);
                let prompt = (!text.is_empty()).then(|| text.to_string());
                (kind, prompt)
            } else {
                (RunKind::Main, None)
            };
            self.upsert_execution_run(conn, session_id, call, run_kind, prompt_text)?;
        }
        Ok(())
    }

    fn upsert_execution_run(
        &self,
        conn: &Connection,
        session_id: &str,
        call: &crate::domain::model_call::ModelCallRow,
        run_kind: RunKind,
        prompt_text: Option<String>,
    ) -> SessionResult<()> {
        let interaction_id = self.ensure_interaction_for_run(conn, session_id, &run_kind, prompt_text.as_deref())?;
        let run_id = format!("run-{session_id}-{}-{}", run_kind.as_str(), call.sequence_no);
        conn.execute(
            "INSERT OR IGNORE INTO execution_runs (
                id, session_id, interaction_id, run_kind, started_at, status
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                run_id,
                session_id,
                interaction_id,
                run_kind.as_str(),
                call.started_at,
                call.status.as_str(),
            ],
        )?;
        // Link the model call to its execution run.
        conn.execute(
            "UPDATE model_calls SET execution_run_id = ?2 WHERE id = ?1",
            params![call.id, run_id],
        )?;
        Ok(())
    }

    fn ensure_interaction_for_run(
        &self,
        conn: &Connection,
        session_id: &str,
        run_kind: &RunKind,
        prompt_text: Option<&str>,
    ) -> SessionResult<Option<String>> {
        if *run_kind == RunKind::Main {
            // A real user prompt maps to the prompt-backed interaction if any;
            // otherwise create one from the model call's prompt text.
            let existing: Option<String> = conn
                .query_row(
                    "SELECT id FROM interactions WHERE session_id = ?1 ORDER BY started_at ASC LIMIT 1",
                    params![session_id],
                    |row| row.get(0),
                )
                .optional()?;
            if existing.is_some() {
                return Ok(existing);
            }
            match prompt_text {
                Some(text) => {
                    let id = format!("inter-{session_id}-main");
                    let now = chrono::Utc::now().timestamp_millis();
                    conn.execute(
                        "INSERT OR IGNORE INTO interactions (
                            id, session_id, prompt_text, started_at, status
                         ) VALUES (?1, ?2, ?3, ?4, 'completed')",
                        params![id, session_id, text, now],
                    )?;
                    Ok(Some(id))
                }
                None => Ok(None),
            }
        } else {
            // Internal runs (title/memory/recap/subagent) are children of the
            // main interaction; they do not create their own interaction.
            Ok(None)
        }
    }

    // ── Subagent parent links (best effort) ──

    fn apply_agent_parent_links(
        &self,
        conn: &Connection,
        session_id: &str,
        _observations: &[Observation],
    ) -> SessionResult<()> {
        // Best-effort: link subagent runs to the preceding main run by time order.
        let agent_runs: Vec<(String, String, i64)> = {
            let mut stmt = conn.prepare(
                "SELECT id, identity_id, started_at FROM agent_runs
                 WHERE session_id = ?1 AND status = 'completed'
                 ORDER BY started_at ASC",
            )?;
            let rows = stmt.query_map(params![session_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, i64>(2)?))
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        // Find the main identity's most recent run start before each subagent.
        let main_identity = format!("ident-{session_id}-main");
        let main_starts: Vec<i64> = agent_runs
            .iter()
            .filter(|(_, ident, _)| ident == &main_identity)
            .map(|(_, _, at)| *at)
            .collect();
        for (run_id, ident, at) in &agent_runs {
            if ident == &main_identity {
                continue;
            }
            if let Some(parent_at) = main_starts.iter().filter(|t| **t <= *at).next_back() {
                let parent_id = format!("arun-{main_identity}-1");
                conn.execute(
                    "UPDATE agent_runs SET parent_agent_run_id = ?2 WHERE id = ?1",
                    params![run_id, parent_id],
                )?;
                let _ = parent_at;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::observation::{Observation, ObservationKind};
    use crate::persist::repo::{SessionRepo, SessionRepoConfig};

    fn repo() -> (Arc<SessionRepo>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reconciler-test.db");
        let repo = Arc::new(
            SessionRepo::open(SessionRepoConfig {
                database_path: path.clone(),
                ..Default::default()
            })
            .unwrap(),
        );
        (repo, dir)
    }

    fn obs(session_id: &str, seq: usize, kind: ObservationKind) -> Observation {
        Observation {
            event_id: format!("ev-{session_id}-{seq}"),
            session_id: session_id.into(),
            source: "proxy".into(),
            occurred_at: 1_700_000_000_000 + seq as i64,
            received_at: 1_700_000_000_000 + seq as i64,
            source_sequence: Some(seq.to_string()),
            source_version: None,
            payload_hash: format!("hash-{session_id}-{seq}"),
            kind,
        }
    }

    #[test]
    fn reconcile_is_idempotent_and_groups_subagent() {
        let (repo, dir) = repo();
        let sid = "sess-recon";
        // Main prompt + a subagent run (transcript pattern) + tool calls.
        let start_main = ObservationKind::ModelCallStart {
            call_id: "call-1".into(),
            client_request_id: None,
            requested_model: Some("deepseek-v4-flash".into()),
            prompt_text: Some("梳理代码，查找错误".into()),
            started_at: 1_700_000_000_000,
        };
        let start_sub = ObservationKind::ModelCallStart {
            call_id: "call-2".into(),
            client_request_id: None,
            requested_model: Some("deepseek-v4-flash".into()),
            prompt_text: Some("<transcript>\nUser: 梳理代码".into()),
            started_at: 1_700_000_000_100,
        };
        let tool = ObservationKind::ToolEmitted {
            call_id: "call-1".into(),
            tool_use_id: "tool-1".into(),
            tool_name: "Bash".into(),
            started_at: 1_700_000_000_200,
        };
        let agent_start = ObservationKind::AgentStart {
            agent_id: "ag-1".into(),
            agent_type: "general-purpose".into(),
            started_at: 1_700_000_000_300,
        };
        let agent_stop = ObservationKind::AgentStop {
            agent_id: "ag-1".into(),
            ended_at: 1_700_000_000_400,
        };

        for (i, kind) in [start_main.clone(), start_sub.clone(), tool, agent_start, agent_stop]
            .into_iter()
            .enumerate()
        {
            repo.record_observation(&obs(sid, i, kind)).unwrap();
        }
        repo.materialize(sid).unwrap();

        let reconciler = Reconciler::new(repo.clone(), ReconcilerConfig::default());
        reconciler.reconcile(sid).unwrap();
        reconciler.reconcile(sid).unwrap(); // idempotent

        repo.with_conn(|conn| {
            let interactions: i64 = conn
                .query_row("SELECT COUNT(*) FROM interactions WHERE session_id = ?1", [sid], |r| r.get(0))
                .unwrap();
            assert_eq!(interactions, 1, "main prompt → one interaction");

            let agent_identities: i64 = conn
                .query_row("SELECT COUNT(*) FROM agent_identities WHERE session_id = ?1", [sid], |r| r.get(0))
                .unwrap();
            // synthetic main + external ag-1
            assert_eq!(agent_identities, 2);

            let runs: i64 = conn
                .query_row("SELECT COUNT(*) FROM agent_runs WHERE session_id = ?1", [sid], |r| r.get(0))
                .unwrap();
            assert_eq!(runs, 1);

            let sub_run_kind: String = conn
                .query_row(
                    "SELECT run_kind FROM execution_runs WHERE session_id = ?1 AND id LIKE '%-subagent-%'",
                    [sid],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(sub_run_kind, "subagent");
            Ok(())
        })
        .unwrap();
        drop(repo);
        let _ = dir;
    }
}
