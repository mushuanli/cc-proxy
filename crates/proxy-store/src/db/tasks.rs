use proxy_common::{NormalizedResponse, SessionId, TaskId, TaskStatus};
use rusqlite::{params, Connection};

use crate::error::StoreResult;
use crate::models::{NewTask, Task, TaskListItem};

/// Insert a task. Returns true if inserted (false if id conflict = idempotent).
pub fn insert_task(
    conn: &Connection,
    task: &NewTask,
    id: &TaskId,
    session_id: &SessionId,
    sequence_no: u64,
    cost_microusd: i64,
) -> StoreResult<bool> {
    let now_ms = chrono::Utc::now().timestamp_millis();
    let response_body_json = task
        .response_body
        .as_ref()
        .map(serde_json::to_string)
        .transpose()?;

    let request_headers_json = task
        .request_headers
        .as_ref()
        .map(serde_json::to_string)
        .transpose()?;

    let response_headers_json = task
        .response_headers
        .as_ref()
        .map(serde_json::to_string)
        .transpose()?;

    let metadata_json = serde_json::to_string(&task.metadata)?;
    let current_operation = crate::persisted_operation_preview(task.summary_json.as_deref());
    let prompt_text = task.prompt_text.clone().or_else(|| {
        task.request_body
            .as_deref()
            .and_then(|body| serde_json::from_str(body).ok())
            .and_then(|body| crate::summary::analyzer::extract_latest_user_prompt(&body))
    });

    let rows = conn.execute(
        "INSERT INTO tasks (
            id, session_id, sequence_no,
            created_at, started_at, first_byte_at, ended_at,
            status, method, path,
            http_status_code, is_streaming,
            requested_model, provider, pricing_model_id,
            resolved_model, upstream,
            input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens,
            duration_ms, ttft_ms, stop_reason, upstream_message_id,
            error_type, error_message,
            input_rate_microusd, output_rate_microusd,
            cache_write_rate_microusd, cache_read_rate_microusd,
            cost_microusd, currency,
            messages_count, prompt_text, current_operation
        ) VALUES (
            ?1, ?2, ?3,
            ?4, ?5, ?6, ?7,
            ?8, ?9, ?10,
            ?11, ?12,
            ?13, ?14, ?15,
            ?16, ?17,
            ?18, ?19, ?20, ?21,
            ?22, ?23, ?24, ?25,
            ?26, ?27,
            ?28, ?29, ?30, ?31,
            ?32, ?33,
            ?34, ?35, ?36
        )
        ON CONFLICT(id) DO NOTHING",
        params![
            id.as_str(),
            session_id.as_str(),
            sequence_no as i64,
            now_ms,
            task.started_at,
            task.first_byte_at,
            task.ended_at,
            task.status.as_str(),
            task.method,
            task.path,
            task.http_status_code.map(|c| c as i64),
            task.is_streaming as i64,
            task.requested_model,
            task.billing.provider,
            task.billing.pricing_model_id,
            task.billing.resolved_model,
            task.upstream,
            task.usage.input_tokens as i64,
            task.usage.output_tokens as i64,
            task.usage.cache_creation_tokens as i64,
            task.usage.cache_read_tokens as i64,
            task.timing.duration_ms,
            task.timing.ttft_ms,
            task.timing.stop_reason,
            task.timing.upstream_message_id,
            task.error
                .as_ref()
                .map(|e| e.error_type.as_str())
                .unwrap_or(""),
            task.error
                .as_ref()
                .map(|e| e.error_message.as_str())
                .unwrap_or(""),
            task.billing.rates.input_microusd,
            task.billing.rates.output_microusd,
            task.billing.rates.cache_write_microusd,
            task.billing.rates.cache_read_microusd,
            cost_microusd,
            task.billing.currency,
            task.messages_count as i64,
            prompt_text,
            current_operation,
        ],
    )?;

    if rows > 0 {
        insert_task_payloads(
            conn,
            id,
            request_headers_json.as_deref(),
            task.request_body.as_deref(),
            response_headers_json.as_deref(),
            response_body_json.as_deref(),
            &metadata_json,
            task.summary_json.as_deref(),
            now_ms,
        )?;
    }
    Ok(rows > 0)
}

#[allow(clippy::too_many_arguments)]
fn insert_task_payloads(
    conn: &Connection,
    id: &TaskId,
    request_headers: Option<&str>,
    request_body: Option<&str>,
    response_headers: Option<&str>,
    response_body: Option<&str>,
    metadata: &str,
    summary: Option<&str>,
    now_ms: i64,
) -> StoreResult<()> {
    conn.execute(
        "INSERT INTO task_details VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            id.as_str(),
            request_headers,
            request_body,
            response_headers,
            response_body,
            metadata
        ],
    )?;
    if let Some(summary) = summary {
        conn.execute(
            "INSERT INTO task_summaries VALUES (?1, 1, ?2, ?3, ?3)",
            params![id.as_str(), summary, now_ms],
        )?;
    }
    Ok(())
}

/// Get a task by id (full detail).
pub fn get_task(conn: &Connection, id: &TaskId) -> StoreResult<Option<Task>> {
    let mut stmt = conn.prepare(
        "SELECT t.*, d.request_headers_json, d.request_body,
         d.response_headers_json, d.response_body, d.metadata_json,
         s.summary_json, s.created_at AS summary_created_at
         FROM tasks t
         LEFT JOIN task_details d ON d.task_id = t.id
         LEFT JOIN task_summaries s ON s.task_id = t.id
         WHERE t.id = ?1",
    )?;

    let mut rows = stmt.query_map(params![id.as_str()], row_to_task)?;
    rows.next().transpose().map_err(Into::into)
}

/// Get full task details for every task in a session (ordered by sequence).
pub fn list_full_tasks(conn: &Connection, session_id: &SessionId) -> StoreResult<Vec<Task>> {
    let mut stmt = conn.prepare(
        "SELECT t.*, d.request_headers_json, d.request_body,
         d.response_headers_json, d.response_body, d.metadata_json,
         s.summary_json, s.created_at AS summary_created_at
         FROM tasks t
         LEFT JOIN task_details d ON d.task_id = t.id
         LEFT JOIN task_summaries s ON s.task_id = t.id
         WHERE t.session_id = ?1
         ORDER BY t.sequence_no ASC",
    )?;

    let rows = stmt.query_map(params![session_id.as_str()], row_to_task)?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

/// List tasks for a session (lightweight, no body).
pub fn list_tasks(
    conn: &Connection,
    session_id: &SessionId,
    time_range: Option<&crate::models::TimeRange>,
    limit: u32,
    before_sequence: Option<u64>,
) -> StoreResult<Vec<TaskListItem>> {
    let mut sql = String::from(
        "SELECT id, sequence_no, started_at, ended_at, status,
         method, path,
         provider, resolved_model, http_status_code,
         input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens,
         cost_microusd, pricing_model_id,
         duration_ms, ttft_ms, messages_count, prompt_text, current_operation
         FROM tasks WHERE session_id = ?1",
    );
    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    params.push(Box::new(session_id.as_str().to_string()));

    if let Some(tr) = time_range {
        if let Some(from) = tr.from {
            params.push(Box::new(from.timestamp_millis()));
            sql.push_str(&format!(" AND started_at >= ?{}", params.len()));
        }
        if let Some(to) = tr.to {
            params.push(Box::new(to.timestamp_millis()));
            sql.push_str(&format!(" AND started_at <= ?{}", params.len()));
        }
    }

    if let Some(sequence) = before_sequence {
        params.push(Box::new(sequence as i64));
        sql.push_str(&format!(" AND sequence_no < ?{}", params.len()));
    }
    params.push(Box::new(limit.min(10_001) as i64));
    sql.push_str(&format!(
        " ORDER BY sequence_no DESC LIMIT ?{}",
        params.len()
    ));

    let params_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_refs.as_slice(), row_to_task_list_item)?;

    let mut result = Vec::new();
    for row in rows {
        result.push(row?);
    }
    Ok(result)
}

/// Update the summary and its persisted list preview.
pub fn update_summary(conn: &Connection, id: &TaskId, summary_json: &str) -> StoreResult<()> {
    let now_ms = chrono::Utc::now().timestamp_millis();
    let preview = crate::persisted_operation_preview(Some(summary_json));
    conn.execute(
        "INSERT INTO task_summaries (task_id, version, summary_json, created_at, updated_at)
         VALUES (?1, 1, ?2, ?3, ?3)
         ON CONFLICT(task_id) DO UPDATE SET
            version = 1, summary_json = excluded.summary_json, updated_at = excluded.updated_at",
        params![id.as_str(), summary_json, now_ms],
    )?;
    conn.execute(
        "UPDATE tasks SET current_operation = ?2 WHERE id = ?1",
        params![id.as_str(), preview],
    )?;
    Ok(())
}

/// Get the latest completed task for a session.
pub fn get_latest_completed_task(
    conn: &Connection,
    session_id: &SessionId,
) -> StoreResult<Option<Task>> {
    let mut stmt = conn.prepare(
        "SELECT t.*, d.request_headers_json, d.request_body,
         d.response_headers_json, d.response_body, d.metadata_json,
         s.summary_json, s.created_at AS summary_created_at
         FROM tasks t
         LEFT JOIN task_details d ON d.task_id = t.id
         LEFT JOIN task_summaries s ON s.task_id = t.id
         WHERE t.session_id = ?1 AND t.ended_at IS NOT NULL AND t.status != 'recording'
         ORDER BY t.sequence_no DESC LIMIT 1",
    )?;

    let mut rows = stmt.query_map(params![session_id.as_str()], row_to_task)?;
    rows.next().transpose().map_err(Into::into)
}

/// Delete old tasks that have been archived and are past retention.
pub fn cleanup_old_tasks(
    conn: &Connection,
    session_id: &SessionId,
    last_archived_sequence: u64,
    retention_cutoff_ms: i64,
) -> StoreResult<usize> {
    let deleted = conn.execute(
        "DELETE FROM tasks
         WHERE session_id = ?1
           AND sequence_no <= ?2
           AND ended_at IS NOT NULL
           AND ended_at < ?3
           AND status != 'recording'",
        params![
            session_id.as_str(),
            last_archived_sequence as i64,
            retention_cutoff_ms
        ],
    )?;
    Ok(deleted)
}

/// Delete task detail only. Session and daily aggregates remain historical authority.
pub fn delete_task(conn: &Connection, id: &TaskId) -> StoreResult<bool> {
    Ok(conn.execute("DELETE FROM tasks WHERE id = ?1", params![id.as_str()])? > 0)
}

pub fn delete_tasks(conn: &Connection, ids: &[TaskId]) -> StoreResult<usize> {
    let mut deleted = 0;
    for id in ids {
        deleted += conn.execute("DELETE FROM tasks WHERE id = ?1", params![id.as_str()])?;
    }
    Ok(deleted)
}

/// Conditionally update a task from Recording to a terminal status.
/// Returns true if a row was affected (transition happened), false if already finalized.
pub fn finalize_task(
    conn: &Connection,
    id: &TaskId,
    finalization: &crate::models::TaskFinalization,
    cost_microusd: i64,
) -> StoreResult<bool> {
    let response_body_json = finalization
        .response_body
        .as_ref()
        .map(serde_json::to_string)
        .transpose()?;

    let response_headers_json = finalization
        .response_headers
        .as_ref()
        .map(serde_json::to_string)
        .transpose()?;

    let metadata_json = serde_json::to_string(&finalization.metadata_patch)?;

    let rows = conn.execute(
        "UPDATE tasks SET
            status = ?2,
            first_byte_at = ?3,
            ended_at = ?4,
            http_status_code = ?5,
            input_tokens = ?6,
            output_tokens = ?7,
            cache_creation_tokens = ?8,
            cache_read_tokens = ?9,
            duration_ms = ?10,
            ttft_ms = ?11,
            stop_reason = ?12,
            upstream_message_id = ?13,
            error_type = ?14,
            error_message = ?15,
            cost_microusd = ?16
         WHERE id = ?1 AND status = 'recording'",
        params![
            id.as_str(),
            finalization.status.as_str(),
            finalization.first_byte_at,
            finalization.ended_at,
            finalization.http_status_code.map(|c| c as i64),
            finalization.usage.input_tokens as i64,
            finalization.usage.output_tokens as i64,
            finalization.usage.cache_creation_tokens as i64,
            finalization.usage.cache_read_tokens as i64,
            finalization.timing.duration_ms,
            finalization.timing.ttft_ms,
            finalization.timing.stop_reason,
            finalization.timing.upstream_message_id,
            finalization
                .error
                .as_ref()
                .map(|e| e.error_type.as_str())
                .unwrap_or(""),
            finalization
                .error
                .as_ref()
                .map(|e| e.error_message.as_str())
                .unwrap_or(""),
            cost_microusd,
        ],
    )?;

    if rows > 0 {
        conn.execute(
            "UPDATE task_details SET response_headers_json = ?2,
             response_body = ?3, metadata_json = ?4 WHERE task_id = ?1",
            params![
                id.as_str(),
                response_headers_json,
                response_body_json,
                metadata_json
            ],
        )?;
    }
    Ok(rows > 0)
}

/// List all tasks currently in Recording status created before the given timestamp.
/// Returns (task_id, session_id) pairs for startup recovery.
pub fn list_recording_tasks(
    conn: &Connection,
    before_ms: i64,
) -> StoreResult<Vec<(TaskId, SessionId)>> {
    let mut stmt = conn.prepare(
        "SELECT id, session_id FROM tasks WHERE status = 'recording' AND created_at < ?1",
    )?;
    let rows = stmt.query_map(params![before_ms], |row| {
        Ok((
            TaskId::new(row.get::<_, String>(0)?),
            SessionId::from_trusted(row.get::<_, String>(1)?),
        ))
    })?;
    let mut result = Vec::new();
    for row in rows {
        result.push(row?);
    }
    Ok(result)
}

fn row_to_task(row: &rusqlite::Row) -> rusqlite::Result<Task> {
    let response_body_str: Option<String> = row.get("response_body")?;
    let response_body =
        response_body_str.and_then(|s| serde_json::from_str::<NormalizedResponse>(&s).ok());

    Ok(Task {
        id: TaskId::new(row.get::<_, String>("id")?),
        session_id: SessionId::from_trusted(row.get::<_, String>("session_id")?),
        sequence_no: row.get::<_, i64>("sequence_no")? as u64,
        created_at: row.get("created_at")?,
        started_at: row.get("started_at")?,
        first_byte_at: row.get("first_byte_at")?,
        ended_at: row.get("ended_at")?,
        status: parse_task_status(&row.get::<_, String>("status")?),
        method: row.get("method")?,
        path: row.get("path")?,
        request_headers: row
            .get::<_, Option<String>>("request_headers_json")?
            .and_then(|s| serde_json::from_str(&s).ok()),
        request_body: row.get("request_body")?,
        response_headers: row
            .get::<_, Option<String>>("response_headers_json")?
            .and_then(|s| serde_json::from_str(&s).ok()),
        response_body,
        http_status_code: row
            .get::<_, Option<i64>>("http_status_code")?
            .map(|v| v as u16),
        is_streaming: row.get::<_, i64>("is_streaming")? != 0,
        requested_model: row.get("requested_model")?,
        provider: row.get::<_, String>("provider")?,
        pricing_model_id: row.get("pricing_model_id")?,
        resolved_model: row.get::<_, String>("resolved_model")?,
        upstream: row.get("upstream")?,
        input_tokens: row.get::<_, i64>("input_tokens")? as u64,
        output_tokens: row.get::<_, i64>("output_tokens")? as u64,
        cache_creation_tokens: row.get::<_, i64>("cache_creation_tokens")? as u64,
        cache_read_tokens: row.get::<_, i64>("cache_read_tokens")? as u64,
        duration_ms: row.get("duration_ms")?,
        ttft_ms: row.get("ttft_ms")?,
        stop_reason: row.get("stop_reason")?,
        upstream_message_id: row.get("upstream_message_id")?,
        error_type: row.get("error_type")?,
        error_message: row.get("error_message")?,
        input_rate_microusd: row.get("input_rate_microusd")?,
        output_rate_microusd: row.get("output_rate_microusd")?,
        cache_write_rate_microusd: row.get("cache_write_rate_microusd")?,
        cache_read_rate_microusd: row.get("cache_read_rate_microusd")?,
        cost_microusd: row.get("cost_microusd")?,
        currency: row.get::<_, String>("currency")?,
        summary_json: row.get("summary_json")?,
        summary_created_at: row.get("summary_created_at")?,
        prompt_text: row.get("prompt_text")?,
        current_operation: row.get("current_operation")?,
        metadata: row
            .get::<_, String>("metadata_json")
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default(),
        messages_count: row.get::<_, i64>("messages_count")? as u32,
    })
}

fn row_to_task_list_item(row: &rusqlite::Row) -> rusqlite::Result<TaskListItem> {
    Ok(TaskListItem {
        id: TaskId::new(row.get::<_, String>("id")?),
        sequence_no: row.get::<_, i64>("sequence_no")? as u64,
        started_at: row.get("started_at")?,
        ended_at: row.get("ended_at")?,
        status: row.get("status")?,
        method: row.get("method")?,
        path: row.get("path")?,
        provider: row.get("provider")?,
        resolved_model: row.get("resolved_model")?,
        http_status_code: row
            .get::<_, Option<i64>>("http_status_code")?
            .map(|v| v as u16),
        input_tokens: row.get::<_, i64>("input_tokens")? as u64,
        output_tokens: row.get::<_, i64>("output_tokens")? as u64,
        cache_creation_tokens: row.get::<_, i64>("cache_creation_tokens")? as u64,
        cache_read_tokens: row.get::<_, i64>("cache_read_tokens")? as u64,
        cost_microusd: row.get("cost_microusd")?,
        priced: row
            .get::<_, Option<String>>("pricing_model_id")?
            .is_some_and(|id| id != "unknown"),
        duration_ms: row.get("duration_ms")?,
        ttft_ms: row.get("ttft_ms")?,
        prompt_text: row.get("prompt_text")?,
        current_operation: row.get("current_operation")?,
        messages_count: row.get::<_, i64>("messages_count")? as u32,
    })
}

fn parse_task_status(s: &str) -> TaskStatus {
    match s {
        "completed" => TaskStatus::Completed,
        "failed" => TaskStatus::Failed,
        "cancelled" => TaskStatus::Cancelled,
        "interrupted" => TaskStatus::Interrupted,
        _ => TaskStatus::Recording,
    }
}
