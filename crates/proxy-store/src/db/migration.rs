use rusqlite::Connection;

use crate::error::StoreResult;

/// Run schema migrations. Creates tables if they don't exist.
pub fn migrate(conn: &Connection) -> StoreResult<()> {
    conn.execute_batch(CREATE_SESSIONS)?;
    conn.execute_batch(CREATE_TASKS)?;
    conn.execute_batch(CREATE_SESSION_DAILY_USAGE)?;
    migrate_v2_add_messages_count(conn)?;
    migrate_v3_session_authority(conn)?;
    migrate_v4_session_status(conn)?;
    migrate_v5_session_diagnostics(conn)?;
    Ok(())
}

/// Migration v5: retain the latest task outcome and timing counters after task cleanup.
fn migrate_v5_session_diagnostics(conn: &Connection) -> StoreResult<()> {
    if conn
        .prepare("SELECT last_task_id FROM sessions LIMIT 0")
        .is_err()
    {
        conn.execute_batch(
            "ALTER TABLE sessions ADD COLUMN unpriced_task_count INTEGER NOT NULL DEFAULT 0;
             ALTER TABLE sessions ADD COLUMN total_ttft_ms INTEGER NOT NULL DEFAULT 0;
             ALTER TABLE sessions ADD COLUMN ttft_task_count INTEGER NOT NULL DEFAULT 0;
             ALTER TABLE sessions ADD COLUMN last_task_id TEXT;
             ALTER TABLE sessions ADD COLUMN last_task_status TEXT;
             ALTER TABLE sessions ADD COLUMN last_stop_reason TEXT;
             ALTER TABLE sessions ADD COLUMN last_error_type TEXT;
             ALTER TABLE sessions ADD COLUMN last_error_message TEXT;",
        )?;
    }
    Ok(())
}

/// Migration v2: add messages_count column to tasks table.
fn migrate_v2_add_messages_count(conn: &Connection) -> StoreResult<()> {
    let has_column: bool = conn
        .prepare("SELECT messages_count FROM tasks LIMIT 0")
        .is_ok();
    if !has_column {
        conn.execute_batch(
            "ALTER TABLE tasks ADD COLUMN messages_count INTEGER NOT NULL DEFAULT 0;",
        )?;
    }
    Ok(())
}

/// Migration v3: add session authority columns (ended_at, latest provider/model/upstream, priced count, duration).
fn migrate_v3_session_authority(conn: &Connection) -> StoreResult<()> {
    let has_column: bool = conn
        .prepare("SELECT ended_at FROM sessions LIMIT 0")
        .is_ok();
    if !has_column {
        conn.execute_batch(
            "ALTER TABLE sessions ADD COLUMN ended_at INTEGER;
             ALTER TABLE sessions ADD COLUMN latest_provider TEXT;
             ALTER TABLE sessions ADD COLUMN latest_model TEXT;
             ALTER TABLE sessions ADD COLUMN latest_upstream TEXT;
             ALTER TABLE sessions ADD COLUMN priced_task_count INTEGER NOT NULL DEFAULT 0;
             ALTER TABLE sessions ADD COLUMN total_duration_ms INTEGER NOT NULL DEFAULT 0;",
        )?;
    }
    Ok(())
}

/// Migration v4: add session status column (recording, stopped, archived).
fn migrate_v4_session_status(conn: &Connection) -> StoreResult<()> {
    let has_column: bool = conn.prepare("SELECT status FROM sessions LIMIT 0").is_ok();
    if !has_column {
        conn.execute_batch(
            "ALTER TABLE sessions ADD COLUMN status TEXT NOT NULL DEFAULT 'recording';",
        )?;
    }
    Ok(())
}

const CREATE_SESSIONS: &str = r#"
CREATE TABLE IF NOT EXISTS sessions (
    id                          TEXT PRIMARY KEY,

    client_type                 INTEGER NOT NULL DEFAULT 0,
    client_session_id           TEXT,

    name                        TEXT,
    cwd                         TEXT,
    project_key                 TEXT,

    created_at                  INTEGER NOT NULL,
    first_activity_at           INTEGER NOT NULL,
    last_activity_at            INTEGER NOT NULL,

    task_count                  INTEGER NOT NULL DEFAULT 0,
    completed_task_count        INTEGER NOT NULL DEFAULT 0,
    failed_task_count           INTEGER NOT NULL DEFAULT 0,

    total_input_tokens          INTEGER NOT NULL DEFAULT 0,
    total_output_tokens         INTEGER NOT NULL DEFAULT 0,
    total_cache_creation_tokens INTEGER NOT NULL DEFAULT 0,
    total_cache_read_tokens     INTEGER NOT NULL DEFAULT 0,

    total_cost_microusd         INTEGER NOT NULL DEFAULT 0,
    currency                    TEXT NOT NULL DEFAULT 'USD',

    next_task_sequence          INTEGER NOT NULL DEFAULT 1,

    last_archived_at            INTEGER,
    last_archived_task_id       TEXT,
    last_archived_sequence      INTEGER NOT NULL DEFAULT 0,
    archive_dirty               INTEGER NOT NULL DEFAULT 1,

    status                      TEXT NOT NULL DEFAULT 'recording',

    -- Session authority state (survives task cleanup)
    ended_at                    INTEGER,
    latest_provider             TEXT,
    latest_model                TEXT,
    latest_upstream             TEXT,
    priced_task_count           INTEGER NOT NULL DEFAULT 0,
    unpriced_task_count         INTEGER NOT NULL DEFAULT 0,
    total_duration_ms           INTEGER NOT NULL DEFAULT 0,
    total_ttft_ms               INTEGER NOT NULL DEFAULT 0,
    ttft_task_count             INTEGER NOT NULL DEFAULT 0,
    last_task_id                TEXT,
    last_task_status            TEXT,
    last_stop_reason            TEXT,
    last_error_type             TEXT,
    last_error_message          TEXT,

    metadata_json               TEXT NOT NULL DEFAULT '{}',

    CHECK (task_count >= 0),
    CHECK (completed_task_count >= 0),
    CHECK (failed_task_count >= 0),
    CHECK (total_input_tokens >= 0),
    CHECK (total_output_tokens >= 0),
    CHECK (total_cache_creation_tokens >= 0),
    CHECK (total_cache_read_tokens >= 0),
    CHECK (total_cost_microusd >= 0),
    CHECK (archive_dirty IN (0, 1)),
    CHECK (status IN ('recording', 'stopped', 'archived'))
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_sessions_client
    ON sessions(client_type, client_session_id)
    WHERE client_session_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_sessions_name
    ON sessions(name);

CREATE INDEX IF NOT EXISTS idx_sessions_last_activity
    ON sessions(last_activity_at DESC);

CREATE INDEX IF NOT EXISTS idx_sessions_project
    ON sessions(project_key, last_activity_at DESC);

CREATE INDEX IF NOT EXISTS idx_sessions_archive_dirty
    ON sessions(archive_dirty, last_activity_at DESC);
"#;

const CREATE_TASKS: &str = r#"
CREATE TABLE IF NOT EXISTS tasks (
    id                          TEXT PRIMARY KEY,
    session_id                  TEXT NOT NULL,

    sequence_no                 INTEGER NOT NULL,

    created_at                  INTEGER NOT NULL,
    started_at                  INTEGER NOT NULL,
    first_byte_at               INTEGER,
    ended_at                    INTEGER,

    status                      TEXT NOT NULL DEFAULT 'recording',

    method                      TEXT NOT NULL,
    path                        TEXT NOT NULL,

    request_headers_json        TEXT,
    request_body                TEXT,

    response_headers_json       TEXT,

    -- SSE parsed, merged, sanitized response
    response_body               TEXT,

    http_status_code            INTEGER,
    is_streaming                INTEGER NOT NULL DEFAULT 0,

    requested_model             TEXT,

    provider                    TEXT NOT NULL DEFAULT 'unknown',
    pricing_model_id            TEXT,
    resolved_model              TEXT NOT NULL DEFAULT 'unknown',
    upstream                    TEXT,

    input_tokens                INTEGER NOT NULL DEFAULT 0,
    output_tokens               INTEGER NOT NULL DEFAULT 0,
    cache_creation_tokens       INTEGER NOT NULL DEFAULT 0,
    cache_read_tokens           INTEGER NOT NULL DEFAULT 0,

    duration_ms                 INTEGER,
    ttft_ms                     INTEGER,

    stop_reason                 TEXT,
    upstream_message_id         TEXT,

    error_type                  TEXT,
    error_message               TEXT,

    -- Price snapshot at task creation: micro-USD / 1,000,000 tokens
    input_rate_microusd         INTEGER NOT NULL DEFAULT 0,
    output_rate_microusd        INTEGER NOT NULL DEFAULT 0,
    cache_write_rate_microusd   INTEGER NOT NULL DEFAULT 0,
    cache_read_rate_microusd    INTEGER NOT NULL DEFAULT 0,

    -- Pre-computed final cost
    cost_microusd               INTEGER NOT NULL DEFAULT 0,
    currency                    TEXT NOT NULL DEFAULT 'USD',

    summary_json                TEXT,
    summary_created_at          INTEGER,

    metadata_json               TEXT NOT NULL DEFAULT '{}',

    FOREIGN KEY (session_id)
        REFERENCES sessions(id)
        ON DELETE CASCADE,

    UNIQUE (session_id, sequence_no),

    CHECK (
        status IN (
            'recording',
            'completed',
            'failed',
            'cancelled',
            'interrupted'
        )
    ),
    CHECK (is_streaming IN (0, 1)),
    CHECK (input_tokens >= 0),
    CHECK (output_tokens >= 0),
    CHECK (cache_creation_tokens >= 0),
    CHECK (cache_read_tokens >= 0),
    CHECK (input_rate_microusd >= 0),
    CHECK (output_rate_microusd >= 0),
    CHECK (cache_write_rate_microusd >= 0),
    CHECK (cache_read_rate_microusd >= 0),
    CHECK (cost_microusd >= 0)
);

CREATE INDEX IF NOT EXISTS idx_tasks_session_sequence
    ON tasks(session_id, sequence_no DESC);

CREATE INDEX IF NOT EXISTS idx_tasks_session_time
    ON tasks(session_id, started_at DESC);

CREATE INDEX IF NOT EXISTS idx_tasks_time
    ON tasks(started_at DESC);

CREATE INDEX IF NOT EXISTS idx_tasks_provider_time
    ON tasks(provider, started_at DESC);

CREATE INDEX IF NOT EXISTS idx_tasks_provider_model_time
    ON tasks(provider, resolved_model, started_at DESC);

CREATE INDEX IF NOT EXISTS idx_tasks_status_time
    ON tasks(status, started_at DESC);
"#;

const CREATE_SESSION_DAILY_USAGE: &str = r#"
CREATE TABLE IF NOT EXISTS session_daily_usage (
    usage_date                  TEXT NOT NULL,

    session_id                  TEXT NOT NULL,
    provider                    TEXT NOT NULL DEFAULT 'unknown',
    model                       TEXT NOT NULL DEFAULT 'unknown',
    currency                    TEXT NOT NULL DEFAULT 'USD',

    task_count                  INTEGER NOT NULL DEFAULT 0,
    completed_task_count        INTEGER NOT NULL DEFAULT 0,
    failed_task_count           INTEGER NOT NULL DEFAULT 0,

    input_tokens                INTEGER NOT NULL DEFAULT 0,
    output_tokens               INTEGER NOT NULL DEFAULT 0,
    cache_creation_tokens       INTEGER NOT NULL DEFAULT 0,
    cache_read_tokens           INTEGER NOT NULL DEFAULT 0,

    cost_microusd               INTEGER NOT NULL DEFAULT 0,

    PRIMARY KEY (
        usage_date,
        session_id,
        provider,
        model,
        currency
    ),

    FOREIGN KEY (session_id)
        REFERENCES sessions(id)
        ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_daily_usage_date
    ON session_daily_usage(usage_date DESC);

CREATE INDEX IF NOT EXISTS idx_daily_usage_session_date
    ON session_daily_usage(session_id, usage_date DESC);

CREATE INDEX IF NOT EXISTS idx_daily_usage_provider_date
    ON session_daily_usage(provider, usage_date DESC);

CREATE INDEX IF NOT EXISTS idx_daily_usage_model_date
    ON session_daily_usage(model, usage_date DESC);

CREATE INDEX IF NOT EXISTS idx_daily_usage_session_provider_date
    ON session_daily_usage(session_id, provider, usage_date DESC);
"#;
