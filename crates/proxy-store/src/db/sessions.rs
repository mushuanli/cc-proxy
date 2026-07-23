use proxy_common::{ClientType, SessionId, TaskId};
use rusqlite::{params, Connection};

use crate::error::StoreResult;
use crate::models::{NewSessionDefaults, Session, SessionFilter, SessionListItem};

/// Helper: read client_type column (INTEGER or legacy TEXT).
fn read_client_type(row: &rusqlite::Row) -> rusqlite::Result<ClientType> {
    // Try integer first (preferred)
    if let Ok(v) = row.get::<_, i64>("client_type") {
        return Ok(ClientType::from_i64(v));
    }
    // Fallback to legacy TEXT
    let s: String = row.get("client_type")?;
    Ok(ClientType::try_from(s.as_str()).unwrap_or_default())
}

/// Create a session if it doesn't already exist. Returns true if created.
pub fn ensure_session(
    conn: &Connection,
    id: &SessionId,
    defaults: &NewSessionDefaults,
    now_ms: i64,
) -> StoreResult<bool> {
    // First check if session already exists by id
    if get_session(conn, id)?.is_some() {
        return Ok(false);
    }

    // Attempt insert (handles PK conflict; unique index conflict should not
    // normally fire because client_session_id == id, but catch it gracefully)
    let result = conn.execute(
        "INSERT INTO sessions (id, client_type, client_session_id, name, cwd, project_key,
         created_at, first_activity_at, last_activity_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT(id) DO NOTHING",
        params![
            id.as_str(),
            defaults.client_type.to_i64(),
            defaults.client_session_id.as_deref().unwrap_or(""),
            defaults.name.as_deref().unwrap_or(""),
            defaults.cwd.as_deref().unwrap_or(""),
            defaults.project_key.as_deref().unwrap_or(""),
            now_ms,
            now_ms,
            now_ms,
        ],
    );

    match result {
        Ok(rows) => {
            if rows > 0 {
                return Ok(true);
            }
            // PK conflict — session already exists, that's fine
            Ok(false)
        }
        Err(rusqlite::Error::SqliteFailure(err, _))
            if err.code == rusqlite::ErrorCode::ConstraintViolation =>
        {
            // Unique index (client_type, client_session_id) conflict
            // Session with same pair exists under a different id — this is
            // unusual but safe to ignore since the caller's session_id will
            // differ from the stored one; subsequent operations reference
            // the caller's session_id directly
            tracing::warn!(
                "[sessions] unique constraint on (client_type={:?}, client_session_id={:?}) with id={}, existing session has different id",
                defaults.client_type,
                defaults.client_session_id,
                id,
            );
            Ok(false)
        }
        Err(e) => Err(e.into()),
    }
}

/// Allocate the next task sequence number for a session.
pub fn allocate_sequence(conn: &Connection, session_id: &SessionId) -> StoreResult<u64> {
    let seq: i64 = conn.query_row(
        "UPDATE sessions SET next_task_sequence = next_task_sequence + 1
         WHERE id = ?1
         RETURNING next_task_sequence - 1",
        params![session_id.as_str()],
        |row| row.get(0),
    )?;
    Ok(seq as u64)
}

/// Update session aggregates after a task write.
#[allow(clippy::too_many_arguments)]
pub fn update_aggregates(
    conn: &Connection,
    session_id: &SessionId,
    status: &str,
    input_tokens: u64,
    output_tokens: u64,
    cache_creation_tokens: u64,
    cache_read_tokens: u64,
    cost_microusd: i64,
    activity_at: i64,
    provider: &str,
    resolved_model: &str,
    upstream: Option<&str>,
    priced: bool,
    duration_ms: i64,
    ttft_ms: Option<i64>,
    ended_at: Option<i64>,
    task_id: &TaskId,
    stop_reason: Option<&str>,
    error_type: Option<&str>,
    error_message: Option<&str>,
) -> StoreResult<()> {
    conn.execute(
        "UPDATE sessions SET
            created_at = MIN(created_at, ?2),
            first_activity_at = MIN(first_activity_at, ?2),
            last_activity_at = MAX(last_activity_at, ?2),
            task_count = task_count + 1,
            completed_task_count = completed_task_count +
                CASE WHEN ?3 = 'completed' THEN 1 ELSE 0 END,
            failed_task_count = failed_task_count +
                CASE WHEN ?3 = 'failed' THEN 1 ELSE 0 END,
            total_input_tokens = total_input_tokens + ?4,
            total_output_tokens = total_output_tokens + ?5,
            total_cache_creation_tokens = total_cache_creation_tokens + ?6,
            total_cache_read_tokens = total_cache_read_tokens + ?7,
            total_cost_microusd = total_cost_microusd + ?8,
            priced_task_count = priced_task_count + CASE WHEN ?9 THEN 1 ELSE 0 END,
            unpriced_task_count = unpriced_task_count + CASE WHEN ?9 THEN 0 ELSE 1 END,
            total_duration_ms = total_duration_ms + ?10,
            total_ttft_ms = total_ttft_ms + COALESCE(?15, 0),
            ttft_task_count = ttft_task_count + CASE WHEN ?15 IS NULL THEN 0 ELSE 1 END,
            latest_provider = ?11,
            latest_model = ?12,
            latest_upstream = COALESCE(?13, latest_upstream),
            ended_at = CASE
                WHEN ?14 IS NULL THEN ended_at
                WHEN ended_at IS NULL THEN ?14
                ELSE MAX(ended_at, ?14)
            END,
            last_task_id = ?16,
            last_task_status = ?3,
            last_stop_reason = ?17,
            last_error_type = ?18,
            last_error_message = ?19,
            archive_dirty = 1
         WHERE id = ?1",
        params![
            session_id.as_str(),
            activity_at,
            status,
            input_tokens as i64,
            output_tokens as i64,
            cache_creation_tokens as i64,
            cache_read_tokens as i64,
            cost_microusd,
            priced,
            duration_ms,
            provider,
            resolved_model,
            upstream,
            ended_at,
            ttft_ms,
            task_id.as_str(),
            stop_reason,
            error_type,
            error_message,
        ],
    )?;
    Ok(())
}

/// Update session archive checkpoint.
pub fn update_archive_checkpoint(
    conn: &Connection,
    session_id: &SessionId,
    archived_at: i64,
    task_id: &str,
    sequence_no: u64,
) -> StoreResult<()> {
    conn.execute(
        "UPDATE sessions SET
            last_archived_at = ?2,
            last_archived_task_id = ?3,
            last_archived_sequence = ?4,
            status = 'archived',
            archive_dirty = 0
         WHERE id = ?1 AND last_archived_sequence <= ?4",
        params![
            session_id.as_str(),
            archived_at,
            task_id,
            sequence_no as i64
        ],
    )?;
    Ok(())
}

/// Set archive_dirty flag.
pub fn set_archive_dirty(conn: &Connection, session_id: &SessionId) -> StoreResult<()> {
    conn.execute(
        "UPDATE sessions SET archive_dirty = 1 WHERE id = ?1",
        params![session_id.as_str()],
    )?;
    Ok(())
}

pub fn delete_session(conn: &Connection, session_id: &SessionId) -> StoreResult<bool> {
    Ok(conn.execute(
        "DELETE FROM sessions WHERE id = ?1",
        params![session_id.as_str()],
    )? > 0)
}

/// Monotonic Recording → Stopped transition. Archived sessions never regress.
pub fn stop_session(conn: &Connection, session_id: &SessionId, ended_at: i64) -> StoreResult<bool> {
    Ok(conn.execute(
        "UPDATE sessions
         SET status = CASE WHEN status = 'recording' THEN 'stopped' ELSE status END,
             ended_at = CASE
                 WHEN ended_at IS NULL THEN ?2
                 ELSE MAX(ended_at, ?2)
             END,
             last_activity_at = MAX(last_activity_at, ?2),
             archive_dirty = 1
         WHERE id = ?1",
        params![session_id.as_str(), ended_at],
    )? > 0)
}

/// Get a session by id.
pub fn get_session(conn: &Connection, id: &SessionId) -> StoreResult<Option<Session>> {
    let mut stmt = conn.prepare(
        "SELECT id, client_type, client_session_id, name, cwd, project_key,
         created_at, first_activity_at, last_activity_at, status,
         task_count, completed_task_count, failed_task_count,
         total_input_tokens, total_output_tokens,
         total_cache_creation_tokens, total_cache_read_tokens,
         total_cost_microusd, currency,
         next_task_sequence, last_archived_at, last_archived_task_id,
         last_archived_sequence, archive_dirty,
         ended_at, latest_provider, latest_model, latest_upstream,
         priced_task_count, unpriced_task_count, total_duration_ms,
         total_ttft_ms, ttft_task_count, last_task_id, last_task_status,
         last_stop_reason, last_error_type, last_error_message, metadata_json
         FROM sessions WHERE id = ?1",
    )?;

    let mut rows = stmt.query_map(params![id.as_str()], row_to_session)?;
    match rows.next() {
        Some(Ok(s)) => Ok(Some(s)),
        _ => Ok(None),
    }
}

/// List sessions with optional filters.
pub fn list_sessions(
    conn: &Connection,
    filter: &SessionFilter,
) -> StoreResult<Vec<SessionListItem>> {
    let mut sql = String::from(
        "SELECT id, client_type, client_session_id, name, cwd, project_key,
         created_at, last_activity_at, task_count,
         total_input_tokens, total_output_tokens,
         total_cache_creation_tokens, total_cache_read_tokens,
         total_cost_microusd, archive_dirty, last_archived_sequence
         FROM sessions WHERE 1=1",
    );
    let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

    if let Some(ref q) = filter.id_or_name {
        param_values.push(Box::new(format!("%{}%", q)));
        sql.push_str(&format!(
            " AND (id LIKE ?{} OR name LIKE ?{})",
            param_values.len(),
            param_values.len()
        ));
    }
    if let Some(ref ct) = filter.client_type {
        param_values.push(Box::new(ct.to_i64()));
        sql.push_str(&format!(" AND client_type = ?{}", param_values.len()));
    }
    if let Some(ref pk) = filter.project_key {
        param_values.push(Box::new(pk.clone()));
        sql.push_str(&format!(" AND project_key = ?{}", param_values.len()));
    }
    if let Some(ref tr) = filter.time_range {
        if let Some(from) = tr.from {
            param_values.push(Box::new(from.timestamp_millis()));
            sql.push_str(&format!(" AND last_activity_at >= ?{}", param_values.len()));
        }
        if let Some(to) = tr.to {
            param_values.push(Box::new(to.timestamp_millis()));
            sql.push_str(&format!(" AND last_activity_at <= ?{}", param_values.len()));
        }
    }

    sql.push_str(" ORDER BY last_activity_at DESC");

    if let Some(limit) = filter.limit {
        param_values.push(Box::new(limit as i64));
        sql.push_str(&format!(" LIMIT ?{}", param_values.len()));
    }
    if let Some(offset) = filter.offset {
        param_values.push(Box::new(offset as i64));
        sql.push_str(&format!(" OFFSET ?{}", param_values.len()));
    }

    let params_refs: Vec<&dyn rusqlite::types::ToSql> =
        param_values.iter().map(|p| p.as_ref()).collect();
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_refs.as_slice(), row_to_session_list_item)?;

    let mut result = Vec::new();
    for row in rows {
        result.push(row?);
    }
    Ok(result)
}

/// Update session name.
pub fn rename_session(
    conn: &Connection,
    id: &SessionId,
    new_name: Option<&str>,
) -> StoreResult<bool> {
    let rows = conn.execute(
        "UPDATE sessions SET name = ?2, archive_dirty = 1 WHERE id = ?1",
        params![id.as_str(), new_name.unwrap_or("")],
    )?;
    Ok(rows > 0)
}

fn row_to_session(row: &rusqlite::Row) -> rusqlite::Result<Session> {
    let archive_dirty: i32 = row.get("archive_dirty")?;
    Ok(Session {
        id: SessionId::from_trusted(row.get::<_, String>("id")?),
        client_type: read_client_type(row)?,
        client_session_id: row.get::<_, Option<String>>("client_session_id")?,
        name: row.get::<_, Option<String>>("name")?,
        cwd: row.get::<_, Option<String>>("cwd")?,
        project_key: row.get::<_, Option<String>>("project_key")?,
        created_at: row.get("created_at")?,
        first_activity_at: row.get("first_activity_at")?,
        last_activity_at: row.get("last_activity_at")?,
        task_count: row.get::<_, i64>("task_count")? as u64,
        completed_task_count: row.get::<_, i64>("completed_task_count")? as u64,
        failed_task_count: row.get::<_, i64>("failed_task_count")? as u64,
        total_input_tokens: row.get::<_, i64>("total_input_tokens")? as u64,
        total_output_tokens: row.get::<_, i64>("total_output_tokens")? as u64,
        total_cache_creation_tokens: row.get::<_, i64>("total_cache_creation_tokens")? as u64,
        total_cache_read_tokens: row.get::<_, i64>("total_cache_read_tokens")? as u64,
        total_cost_microusd: row.get("total_cost_microusd")?,
        currency: row.get::<_, String>("currency")?,
        next_task_sequence: row.get::<_, i64>("next_task_sequence")? as u64,
        last_archived_at: row.get("last_archived_at")?,
        last_archived_task_id: row
            .get::<_, Option<String>>("last_archived_task_id")?
            .map(TaskId::new),
        last_archived_sequence: row.get::<_, i64>("last_archived_sequence")? as u64,
        archive_dirty: archive_dirty != 0,
        status: row
            .get::<_, String>("status")
            .unwrap_or_else(|_| "recording".into()),
        ended_at: row.get("ended_at")?,
        latest_provider: row.get("latest_provider")?,
        latest_model: row.get("latest_model")?,
        latest_upstream: row.get("latest_upstream")?,
        priced_task_count: row.get::<_, i64>("priced_task_count")? as u64,
        unpriced_task_count: row.get::<_, i64>("unpriced_task_count")? as u64,
        total_duration_ms: row.get("total_duration_ms")?,
        total_ttft_ms: row.get("total_ttft_ms")?,
        ttft_task_count: row.get::<_, i64>("ttft_task_count")? as u64,
        last_task_id: row
            .get::<_, Option<String>>("last_task_id")?
            .map(TaskId::new),
        last_task_status: row.get("last_task_status")?,
        last_stop_reason: row.get("last_stop_reason")?,
        last_error_type: row.get("last_error_type")?,
        last_error_message: row.get("last_error_message")?,
        metadata: row
            .get::<_, String>("metadata_json")
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default(),
    })
}

fn row_to_session_list_item(row: &rusqlite::Row) -> rusqlite::Result<SessionListItem> {
    let archive_dirty: i32 = row.get("archive_dirty")?;
    Ok(SessionListItem {
        id: SessionId::from_trusted(row.get::<_, String>("id")?),
        client_type: read_client_type(row)?,
        client_session_id: row.get("client_session_id")?,
        name: row.get("name")?,
        cwd: row.get("cwd")?,
        project_key: row.get("project_key")?,
        created_at: row.get("created_at")?,
        last_activity_at: row.get("last_activity_at")?,
        task_count: row.get::<_, i64>("task_count")? as u64,
        total_input_tokens: row.get::<_, i64>("total_input_tokens")? as u64,
        total_output_tokens: row.get::<_, i64>("total_output_tokens")? as u64,
        total_cache_creation_tokens: row.get::<_, i64>("total_cache_creation_tokens")? as u64,
        total_cache_read_tokens: row.get::<_, i64>("total_cache_read_tokens")? as u64,
        total_cost_microusd: row.get("total_cost_microusd")?,
        archive_dirty: archive_dirty != 0,
        last_archived_sequence: row.get::<_, i64>("last_archived_sequence")? as u64,
    })
}
