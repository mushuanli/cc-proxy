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
            request_headers_json, request_body,
            response_headers_json, response_body,
            http_status_code, is_streaming,
            requested_model, provider, pricing_model_id,
            resolved_model, upstream,
            input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens,
            duration_ms, ttft_ms, stop_reason, upstream_message_id,
            error_type, error_message,
            input_rate_microusd, output_rate_microusd,
            cache_write_rate_microusd, cache_read_rate_microusd,
            cost_microusd, currency,
            messages_count, metadata_json, prompt_text,
            summary_json, summary_created_at
        ) VALUES (
            ?1, ?2, ?3,
            ?4, ?5, ?6, ?7,
            ?8, ?9, ?10,
            ?11, ?12,
            ?13, ?14,
            ?15, ?16,
            ?17, ?18, ?19,
            ?20, ?21,
            ?22, ?23, ?24, ?25,
            ?26, ?27, ?28, ?29,
            ?30, ?31,
            ?32, ?33, ?34, ?35,
            ?36, ?37,
            ?38, ?39, ?40,
            ?41, ?42
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
            request_headers_json,
            task.request_body,
            response_headers_json,
            response_body_json,
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
            metadata_json,
            prompt_text,
            task.summary_json,
            task.summary_json.as_ref().map(|_| now_ms),
        ],
    )?;

    Ok(rows > 0)
}

/// Get a task by id (full detail).
pub fn get_task(conn: &Connection, id: &TaskId) -> StoreResult<Option<Task>> {
    let mut stmt = conn.prepare(
        "SELECT id, session_id, sequence_no,
         created_at, started_at, first_byte_at, ended_at,
         status, method, path,
         request_headers_json, request_body,
         response_headers_json, response_body,
         http_status_code, is_streaming,
         requested_model, provider, pricing_model_id,
         resolved_model, upstream,
         input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens,
         duration_ms, ttft_ms, stop_reason, upstream_message_id,
         error_type, error_message,
         input_rate_microusd, output_rate_microusd,
         cache_write_rate_microusd, cache_read_rate_microusd,
         cost_microusd, currency,
         summary_json, summary_created_at, metadata_json, messages_count, prompt_text
         FROM tasks WHERE id = ?1",
    )?;

    let mut rows = stmt.query_map(params![id.as_str()], row_to_task)?;
    match rows.next() {
        Some(Ok(t)) => Ok(Some(t)),
        _ => Ok(None),
    }
}

/// List tasks for a session (lightweight, no body).
pub fn list_tasks(
    conn: &Connection,
    session_id: &SessionId,
    time_range: Option<&crate::models::TimeRange>,
) -> StoreResult<Vec<TaskListItem>> {
    let mut sql = String::from(
        "SELECT id, sequence_no, started_at, ended_at, status,
         method, path,
         provider, resolved_model, http_status_code,
         input_tokens, output_tokens, cost_microusd, pricing_model_id,
         duration_ms, ttft_ms, summary_json, messages_count, prompt_text
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

    sql.push_str(" ORDER BY sequence_no DESC");

    let params_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_refs.as_slice(), row_to_task_list_item)?;

    let mut result = Vec::new();
    for row in rows {
        result.push(row?);
    }
    Ok(result)
}

/// Update task summary_json cache.
pub fn update_summary(conn: &Connection, id: &TaskId, summary_json: &str) -> StoreResult<()> {
    let now_ms = chrono::Utc::now().timestamp_millis();
    conn.execute(
        "UPDATE tasks SET summary_json = ?2, summary_created_at = ?3 WHERE id = ?1",
        params![id.as_str(), summary_json, now_ms],
    )?;
    Ok(())
}

/// Get the latest completed task for a session.
pub fn get_latest_completed_task(
    conn: &Connection,
    session_id: &SessionId,
) -> StoreResult<Option<Task>> {
    let mut stmt = conn.prepare(
        "SELECT id, session_id, sequence_no,
         created_at, started_at, first_byte_at, ended_at,
         status, method, path,
         request_headers_json, request_body,
         response_headers_json, response_body,
         http_status_code, is_streaming,
         requested_model, provider, pricing_model_id,
         resolved_model, upstream,
         input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens,
         duration_ms, ttft_ms, stop_reason, upstream_message_id,
         error_type, error_message,
         input_rate_microusd, output_rate_microusd,
         cache_write_rate_microusd, cache_read_rate_microusd,
         cost_microusd, currency,
         summary_json, summary_created_at, metadata_json, messages_count, prompt_text
         FROM tasks
         WHERE session_id = ?1 AND ended_at IS NOT NULL AND status != 'recording'
         ORDER BY sequence_no DESC LIMIT 1",
    )?;

    let mut rows = stmt.query_map(params![session_id.as_str()], row_to_task)?;
    match rows.next() {
        Some(Ok(t)) => Ok(Some(t)),
        _ => Ok(None),
    }
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
        cost_microusd: row.get("cost_microusd")?,
        priced: row
            .get::<_, Option<String>>("pricing_model_id")?
            .is_some_and(|id| id != "unknown"),
        duration_ms: row.get("duration_ms")?,
        ttft_ms: row.get("ttft_ms")?,
        summary_json: row.get("summary_json")?,
        prompt_text: row.get("prompt_text")?,
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
