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
    migrate_v6_prompt_text(conn)?;
    migrate_v7_split_task_payloads(conn)?;
    Ok(())
}

/// Migration v7: keep task list rows narrow and move large payloads to child tables.
fn migrate_v7_split_task_payloads(conn: &Connection) -> StoreResult<()> {
    if conn
        .prepare("SELECT request_body FROM tasks LIMIT 0")
        .is_err()
    {
        conn.execute_batch(CREATE_TASK_DETAILS)?;
        conn.execute_batch(CREATE_TASK_SUMMARIES)?;
        return Ok(());
    }

    let tx = conn.unchecked_transaction()?;
    tx.execute_batch("ALTER TABLE tasks RENAME TO tasks_legacy_v7;")?;
    tx.execute_batch(CREATE_TASKS_V7)?;
    tx.execute_batch(CREATE_TASK_DETAILS)?;
    tx.execute_batch(CREATE_TASK_SUMMARIES)?;
    copy_task_rows(&tx)?;
    copy_task_payloads(&tx)?;
    backfill_operation_previews(&tx)?;
    tx.execute_batch("DROP TABLE tasks_legacy_v7;")?;
    tx.execute_batch(CREATE_TASK_INDEXES_V7)?;
    tx.commit()?;
    Ok(())
}

fn copy_task_rows(conn: &Connection) -> StoreResult<()> {
    conn.execute_batch(
        "INSERT INTO tasks (
            id, session_id, sequence_no, created_at, started_at, first_byte_at, ended_at,
            status, method, path, http_status_code, is_streaming, requested_model,
            provider, pricing_model_id, resolved_model, upstream,
            input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens,
            duration_ms, ttft_ms, stop_reason, upstream_message_id, error_type, error_message,
            input_rate_microusd, output_rate_microusd, cache_write_rate_microusd,
            cache_read_rate_microusd, cost_microusd, currency, messages_count, prompt_text
         )
         SELECT id, session_id, sequence_no, created_at, started_at, first_byte_at, ended_at,
            status, method, path, http_status_code, is_streaming, requested_model,
            provider, pricing_model_id, resolved_model, upstream,
            input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens,
            duration_ms, ttft_ms, stop_reason, upstream_message_id, error_type, error_message,
            input_rate_microusd, output_rate_microusd, cache_write_rate_microusd,
            cache_read_rate_microusd, cost_microusd, currency, messages_count, prompt_text
         FROM tasks_legacy_v7;",
    )?;
    Ok(())
}

fn copy_task_payloads(conn: &Connection) -> StoreResult<()> {
    conn.execute_batch(
        "INSERT INTO task_details (
            task_id, request_headers_json, request_body, response_headers_json,
            response_body, metadata_json
         )
         SELECT id, request_headers_json, request_body, response_headers_json,
            response_body, metadata_json
         FROM tasks_legacy_v7;
         INSERT INTO task_summaries (task_id, version, summary_json, created_at, updated_at)
         SELECT id, 1, summary_json, summary_created_at, summary_created_at
         FROM tasks_legacy_v7 WHERE summary_json IS NOT NULL;",
    )?;
    Ok(())
}

fn backfill_operation_previews(conn: &Connection) -> StoreResult<()> {
    let mut stmt = conn.prepare("SELECT task_id, summary_json FROM task_summaries")?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let summaries = rows.collect::<Result<Vec<_>, _>>()?;
    drop(stmt);
    for (id, summary) in summaries {
        let preview = crate::persisted_operation_preview(Some(&summary));
        conn.execute(
            "UPDATE tasks SET current_operation = ?2 WHERE id = ?1",
            rusqlite::params![id, preview],
        )?;
    }
    Ok(())
}

/// Migration v6: separate the list prompt from the full task summary.
fn migrate_v6_prompt_text(conn: &Connection) -> StoreResult<()> {
    let column_exists = conn
        .prepare("SELECT prompt_text FROM tasks LIMIT 0")
        .is_ok();
    let has_legacy_body = conn.prepare("SELECT request_body FROM tasks LIMIT 0").is_ok();

    let tx = conn.unchecked_transaction()?;
    if !column_exists {
        tx.execute_batch("ALTER TABLE tasks ADD COLUMN prompt_text TEXT;")?;
        // One-time: remove legacy non-v1 summaries
        let mut stmt =
            tx.prepare("SELECT id, summary_json FROM tasks WHERE summary_json IS NOT NULL")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
        })?;
        let records = rows.collect::<Result<Vec<_>, _>>()?;
        drop(stmt);
        for (id, summary_json) in records {
            remove_legacy_summary(&tx, &id, summary_json.as_deref())?;
        }
    }

    if !has_legacy_body {
        tx.commit()?;
        return Ok(());
    }

    // Backfill NULL prompt_text from request_body (idempotent on re-runs)
    let mut stmt = tx.prepare(
        "SELECT id, request_body FROM tasks
         WHERE prompt_text IS NULL AND request_body IS NOT NULL",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
    })?;
    let records = rows.collect::<Result<Vec<_>, _>>()?;
    drop(stmt);
    for (id, body) in records {
        backfill_prompt(&tx, &id, body.as_deref())?;
    }
    tx.commit()?;
    Ok(())
}

fn backfill_prompt(conn: &Connection, id: &str, request_body: Option<&str>) -> StoreResult<()> {
    let prompt = request_body
        .and_then(|body| serde_json::from_str(body).ok())
        .and_then(|body| crate::summary::analyzer::extract_latest_user_prompt(&body));
    conn.execute(
        "UPDATE tasks SET prompt_text = ?2 WHERE id = ?1",
        rusqlite::params![id, prompt],
    )?;
    Ok(())
}

fn remove_legacy_summary(
    conn: &Connection,
    id: &str,
    summary_json: Option<&str>,
) -> StoreResult<()> {
    let is_v1 = summary_json
        .and_then(|json| serde_json::from_str::<serde_json::Value>(json).ok())
        .and_then(|value| value.get("version").and_then(|v| v.as_u64()))
        == Some(1);
    if summary_json.is_some() && !is_v1 {
        conn.execute(
            "UPDATE tasks SET summary_json = NULL, summary_created_at = NULL WHERE id = ?1",
            [id],
        )?;
    }
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
    prompt_text                 TEXT,

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

const CREATE_TASKS_V7: &str = r#"
CREATE TABLE tasks (
    id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL,
    sequence_no INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    started_at INTEGER NOT NULL,
    first_byte_at INTEGER,
    ended_at INTEGER,
    status TEXT NOT NULL DEFAULT 'recording',
    method TEXT NOT NULL,
    path TEXT NOT NULL,
    http_status_code INTEGER,
    is_streaming INTEGER NOT NULL DEFAULT 0,
    requested_model TEXT,
    provider TEXT NOT NULL DEFAULT 'unknown',
    pricing_model_id TEXT,
    resolved_model TEXT NOT NULL DEFAULT 'unknown',
    upstream TEXT,
    input_tokens INTEGER NOT NULL DEFAULT 0,
    output_tokens INTEGER NOT NULL DEFAULT 0,
    cache_creation_tokens INTEGER NOT NULL DEFAULT 0,
    cache_read_tokens INTEGER NOT NULL DEFAULT 0,
    duration_ms INTEGER,
    ttft_ms INTEGER,
    stop_reason TEXT,
    upstream_message_id TEXT,
    error_type TEXT,
    error_message TEXT,
    input_rate_microusd INTEGER NOT NULL DEFAULT 0,
    output_rate_microusd INTEGER NOT NULL DEFAULT 0,
    cache_write_rate_microusd INTEGER NOT NULL DEFAULT 0,
    cache_read_rate_microusd INTEGER NOT NULL DEFAULT 0,
    cost_microusd INTEGER NOT NULL DEFAULT 0,
    currency TEXT NOT NULL DEFAULT 'USD',
    messages_count INTEGER NOT NULL DEFAULT 0,
    prompt_text TEXT,
    current_operation TEXT,
    FOREIGN KEY (session_id) REFERENCES sessions(id) ON DELETE CASCADE,
    UNIQUE (session_id, sequence_no),
    CHECK (status IN ('recording','completed','failed','cancelled','interrupted')),
    CHECK (is_streaming IN (0, 1))
);
"#;

const CREATE_TASK_DETAILS: &str = r#"
CREATE TABLE IF NOT EXISTS task_details (
    task_id TEXT PRIMARY KEY,
    request_headers_json TEXT,
    request_body TEXT,
    response_headers_json TEXT,
    response_body TEXT,
    metadata_json TEXT NOT NULL DEFAULT '{}',
    FOREIGN KEY (task_id) REFERENCES tasks(id) ON DELETE CASCADE
);
"#;

const CREATE_TASK_SUMMARIES: &str = r#"
CREATE TABLE IF NOT EXISTS task_summaries (
    task_id TEXT PRIMARY KEY,
    version INTEGER NOT NULL DEFAULT 1,
    summary_json TEXT NOT NULL,
    created_at INTEGER,
    updated_at INTEGER NOT NULL,
    FOREIGN KEY (task_id) REFERENCES tasks(id) ON DELETE CASCADE
);
"#;

const CREATE_TASK_INDEXES_V7: &str = r#"
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v6_backfills_prompt_and_removes_legacy_summary() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE tasks (
                id TEXT PRIMARY KEY,
                request_body TEXT,
                summary_json TEXT,
                summary_created_at INTEGER
            );
            INSERT INTO tasks VALUES (
                'task-1',
                '{\"messages\":[
                    {\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"commit\"}]},
                    {\"role\":\"user\",\"content\":[{\"type\":\"tool_result\",\"content\":\"ok\"}]}
                ]}',
                '{\"t\":\"tools\"}',
                1
            );",
        )
        .unwrap();

        migrate_v6_prompt_text(&conn).unwrap();
        migrate_v6_prompt_text(&conn).unwrap();

        let migrated: (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT prompt_text, summary_json FROM tasks WHERE id = 'task-1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(migrated, (Some("commit".into()), None));
    }

    #[test]
    fn v7_schema_is_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();

        migrate(&conn).unwrap();
        migrate(&conn).unwrap();

        assert!(conn.prepare("SELECT current_operation FROM tasks").is_ok());
        assert!(conn.prepare("SELECT request_body FROM task_details").is_ok());
        assert!(conn.prepare("SELECT summary_json FROM task_summaries").is_ok());
        assert!(conn.prepare("SELECT request_body FROM tasks").is_err());
    }
}
