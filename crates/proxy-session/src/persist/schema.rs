//! Schema migrations for the proxy-session tables.
//!
//! These tables live in the same SQLite file as the proxy-store tables but
//! are created and owned by this crate (independent connection).

use rusqlite::Connection;

use crate::SessionResult;

/// Run all schema migrations. Idempotent — safe to call on every open.
pub fn migrate(conn: &Connection) -> SessionResult<()> {
    conn.execute_batch(CREATE_OBSERVATIONS)?;
    conn.execute_batch(CREATE_MODEL_CALLS)?;
    conn.execute_batch(CREATE_TOOL_INVOCATIONS)?;
    conn.execute_batch(CREATE_MODEL_ATTEMPTS)?;
    conn.execute_batch(CREATE_INTERACTIONS)?;
    conn.execute_batch(CREATE_EXECUTION_RUNS)?;
    conn.execute_batch(CREATE_AGENT_IDENTITIES)?;
    conn.execute_batch(CREATE_AGENT_RUNS)?;
    Ok(())
}

const CREATE_OBSERVATIONS: &str = r#"
CREATE TABLE IF NOT EXISTS observations (
    event_id                TEXT PRIMARY KEY,
    session_id              TEXT NOT NULL,
    source                  TEXT NOT NULL,
    event_type              TEXT NOT NULL,
    occurred_at             INTEGER NOT NULL,
    received_at             INTEGER NOT NULL,
    source_sequence         TEXT,
    source_version          TEXT,
    payload_hash            TEXT NOT NULL,
    raw_payload             TEXT NOT NULL,
    model_call_id           TEXT,
    agent_id                TEXT,
    prompt_id               TEXT,
    tool_use_id             TEXT
);
CREATE INDEX IF NOT EXISTS idx_obs_session_time ON observations(session_id, received_at);
CREATE UNIQUE INDEX IF NOT EXISTS idx_obs_payload_hash ON observations(session_id, payload_hash);
"#;

const CREATE_MODEL_CALLS: &str = r#"
CREATE TABLE IF NOT EXISTS model_calls (
    id                      TEXT PRIMARY KEY,
    session_id              TEXT NOT NULL,
    sequence_no             INTEGER NOT NULL,
    previous_model_call_id  TEXT,
    client_request_id       TEXT,
    provider_request_id     TEXT,
    started_at              INTEGER NOT NULL DEFAULT 0,
    status                  TEXT NOT NULL DEFAULT 'in_progress',
    requested_model         TEXT,
    resolved_model          TEXT NOT NULL DEFAULT 'unknown',
    provider                TEXT NOT NULL DEFAULT 'unknown',
    upstream                TEXT,
    input_tokens            INTEGER NOT NULL DEFAULT 0,
    output_tokens           INTEGER NOT NULL DEFAULT 0,
    cache_creation_tokens   INTEGER NOT NULL DEFAULT 0,
    cache_read_tokens       INTEGER NOT NULL DEFAULT 0,
    cost_microusd           INTEGER NOT NULL DEFAULT 0,
    duration_ms             INTEGER,
    ttft_ms                 INTEGER,
    stop_reason             TEXT,
    http_status_code        INTEGER,
    error_type              TEXT,
    error_message           TEXT,
    classification_source   TEXT NOT NULL DEFAULT 'heuristic',
    classification_confidence TEXT NOT NULL DEFAULT 'weak',
    classifier_version      TEXT NOT NULL DEFAULT 'claude-code-v2',
    UNIQUE(session_id, sequence_no)
);
CREATE INDEX IF NOT EXISTS idx_mc_session_seq ON model_calls(session_id, sequence_no);
CREATE INDEX IF NOT EXISTS idx_mc_client_req ON model_calls(client_request_id)
    WHERE client_request_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_mc_provider_req ON model_calls(provider_request_id)
    WHERE provider_request_id IS NOT NULL;
"#;

const CREATE_TOOL_INVOCATIONS: &str = r#"
CREATE TABLE IF NOT EXISTS tool_invocations (
    id                      TEXT PRIMARY KEY,
    model_call_id           TEXT NOT NULL,
    tool_use_id             TEXT,
    operation_seq           INTEGER NOT NULL,
    tool_name               TEXT NOT NULL,
    status                  TEXT NOT NULL DEFAULT 'emitted',
    started_at              INTEGER,
    ended_at                INTEGER,
    duration_ms             INTEGER,
    model_input_preview     TEXT,
    effective_input_preview TEXT,
    raw_result_preview      TEXT,
    effective_result_preview TEXT,
    UNIQUE(model_call_id, operation_seq)
);
CREATE INDEX IF NOT EXISTS idx_tool_call ON tool_invocations(model_call_id, operation_seq);
CREATE UNIQUE INDEX IF NOT EXISTS idx_tool_use_id ON tool_invocations(tool_use_id)
    WHERE tool_use_id IS NOT NULL;
"#;

const CREATE_MODEL_ATTEMPTS: &str = r#"
CREATE TABLE IF NOT EXISTS model_attempts (
    id                      TEXT PRIMARY KEY,
    model_call_id           TEXT NOT NULL,
    attempt_no              INTEGER NOT NULL,
    trace_id                TEXT,
    span_id                 TEXT,
    started_at              INTEGER,
    ended_at                INTEGER,
    http_status_code        INTEGER,
    error_type              TEXT,
    error_message           TEXT,
    UNIQUE(model_call_id, attempt_no)
);
"#;

const CREATE_INTERACTIONS: &str = r#"
CREATE TABLE IF NOT EXISTS interactions (
    id                      TEXT PRIMARY KEY,
    session_id              TEXT NOT NULL,
    external_prompt_id      TEXT,
    prompt_text             TEXT,
    started_at              INTEGER NOT NULL,
    ended_at                INTEGER,
    status                  TEXT NOT NULL DEFAULT 'in_progress',
    classification_source   TEXT NOT NULL DEFAULT 'heuristic',
    classification_confidence TEXT NOT NULL DEFAULT 'weak',
    classifier_version      TEXT NOT NULL DEFAULT 'claude-code-v2'
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_interactions_prompt
    ON interactions(session_id, external_prompt_id)
    WHERE external_prompt_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_interactions_session ON interactions(session_id, started_at);
"#;

const CREATE_EXECUTION_RUNS: &str = r#"
CREATE TABLE IF NOT EXISTS execution_runs (
    id                      TEXT PRIMARY KEY,
    session_id              TEXT NOT NULL,
    interaction_id          TEXT,
    run_kind                TEXT NOT NULL DEFAULT 'main',
    agent_run_id            TEXT,
    started_at              INTEGER NOT NULL,
    foreground_completed_at INTEGER,
    settled_at              INTEGER,
    status                  TEXT NOT NULL DEFAULT 'in_progress',
    classification_source   TEXT NOT NULL DEFAULT 'heuristic',
    classification_confidence TEXT NOT NULL DEFAULT 'weak'
);
CREATE INDEX IF NOT EXISTS idx_exec_runs_session ON execution_runs(session_id, started_at);
"#;

const CREATE_AGENT_IDENTITIES: &str = r#"
CREATE TABLE IF NOT EXISTS agent_identities (
    id                      TEXT PRIMARY KEY,
    session_id              TEXT NOT NULL,
    external_agent_id       TEXT,
    agent_type              TEXT NOT NULL DEFAULT 'main',
    synthetic               INTEGER NOT NULL DEFAULT 0
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_agent_identities_external
    ON agent_identities(session_id, external_agent_id)
    WHERE external_agent_id IS NOT NULL;
"#;

const CREATE_AGENT_RUNS: &str = r#"
CREATE TABLE IF NOT EXISTS agent_runs (
    id                          TEXT PRIMARY KEY,
    session_id                  TEXT NOT NULL,
    identity_id                 TEXT NOT NULL,
    run_no                      INTEGER NOT NULL DEFAULT 1,
    parent_agent_run_id         TEXT,
    spawned_by_tool_invocation_id TEXT,
    interaction_id              TEXT,
    started_at                  INTEGER NOT NULL,
    ended_at                    INTEGER,
    status                      TEXT NOT NULL DEFAULT 'in_progress',
    UNIQUE(identity_id, run_no)
);
CREATE INDEX IF NOT EXISTS idx_agent_runs_session ON agent_runs(session_id, started_at);
"#;
