use std::collections::HashSet;
use std::io::{Cursor, Write};
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use chrono::TimeZone;
use proxy_common::{SessionId, TaskId};
use serde::Deserialize;
use serde_json::{json, Value};
use zip::write::FileOptions;
use zip::ZipWriter;

use crate::AppState;

#[derive(Deserialize, Default)]
pub struct ListQuery {
    pub session_id: Option<String>,
    pub limit: Option<u32>,
}

#[derive(Deserialize, Default)]
pub struct SessionTasksQuery {
    pub limit: Option<u32>,
    pub before_sequence: Option<u64>,
}

#[derive(Deserialize, Default)]
pub struct ExportTasksRequest {
    #[serde(default)]
    pub session_ids: Vec<String>,
    #[serde(default)]
    pub ids: Vec<String>,
}

/// Transform TaskListItem to match frontend expectations (session_id, timestamp, model, cost).
pub(crate) fn task_to_json(
    task: &proxy_store::TaskListItem,
    session_id: Option<&proxy_common::SessionId>,
) -> Value {
    json!({
        "id": task.id,
        "session_id": session_id.map(|s| s.as_str()),
        "sequence_no": task.sequence_no,
        "timestamp": task.started_at,
        "started_at": task.started_at,
        "ended_at": task.ended_at,
        "status": task.status,
        "method": task.method,
        "path": task.path,
        "provider": task.provider,
        "model": task.resolved_model,
        "resolved_model": task.resolved_model,
        "http_status_code": task.http_status_code,
        "status_code": task.http_status_code,
        "input_tokens": task.input_tokens,
        "output_tokens": task.output_tokens,
        "cache_creation_input_tokens": task.cache_creation_tokens,
        "cache_read_input_tokens": task.cache_read_tokens,
        "cost_microusd": task.cost_microusd,
        "cost": task.cost_microusd as f64 / 1_000_000.0,
        "priced": task.priced,
        "duration_ms": task.duration_ms,
        "time_to_first_token_ms": task.ttft_ms,
        "ttft_ms": task.ttft_ms,
        "prompt": task.prompt_text,
        "current_operation": task.current_operation,
        "messages_count": task.messages_count,
    })
}

/// Transform full Task to ProxiedRequest-compatible JSON for detail view.
pub(crate) fn task_to_full_json(task: &proxy_store::Task) -> Value {
    let raw_response = task
        .metadata
        .get("raw_response_body")
        .and_then(|v| v.as_str());
    let inspected_response = raw_response
        .and_then(|body| serde_json::from_str::<Value>(body).ok())
        .unwrap_or_else(|| json!(task.response_body));
    let sse_events = task
        .metadata
        .get("sse_events")
        .cloned()
        .unwrap_or_else(|| json!([]));
    let content_text = task
        .response_body
        .as_ref()
        .map(|body| body.text.join(""))
        .filter(|body| !body.is_empty());
    // Forward request_headers / response_headers as raw JSON objects
    // so formatHeaders() in frontend (Object.entries) works.
    json!({
        "id": task.id,
        "session_id": task.session_id,
        "sequence_no": task.sequence_no,
        "timestamp": task.started_at,
        "started_at": task.started_at,
        "created_at": task.created_at,
        "ended_at": task.ended_at,
        "status": task.status,
        "method": task.method,
        "path": task.path,
        "provider": task.provider,
        "model": task.resolved_model,
        "resolved_model": task.resolved_model,
        "requested_model": task.requested_model,
        "http_status_code": task.http_status_code,
        "status_code": task.http_status_code,
        "input_tokens": task.input_tokens,
        "output_tokens": task.output_tokens,
        "cache_creation_input_tokens": task.cache_creation_tokens,
        "cache_read_input_tokens": task.cache_read_tokens,
        "cost_microusd": task.cost_microusd,
        "cost": task.cost_microusd as f64 / 1_000_000.0,
        "priced": task.pricing_model_id.as_deref().is_some_and(|id| id != "unknown"),
        "duration_ms": task.duration_ms,
        "time_to_first_token_ms": task.ttft_ms,
        "ttft_ms": task.ttft_ms,
        "stop_reason": task.stop_reason,
        "is_streaming": task.is_streaming,
        "upstream": task.upstream,
        "error_type": task.error_type,
        "error_message": task.error_message,
        // Headers & body
        "request_headers": task.request_headers,
        "request_body": task.request_body,
        "response_headers": task.response_headers,
        // response_body is NormalizedResponse — serialize as-is; frontend jsonTreeHTML handles objects
        "response_body": inspected_response,
        "normalized_response": task.response_body,
        "content_text": content_text,
        "sse_events": sse_events,
        "request_type": task.metadata.get("protocol").and_then(|v| v.as_str()).unwrap_or("anthropic"),
        "messages_count": task.messages_count,
        "prompt": task.prompt_text,
        "current_operation": task.current_operation,
        "summary_json": task.summary_json
    })
}

pub async fn list_session_tasks(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<SessionTasksQuery>,
) -> impl IntoResponse {
    let sid = match SessionId::new(id) {
        Ok(value) => value,
        Err(e) => return (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response(),
    };
    let limit = q.limit.unwrap_or(50).clamp(1, 200);
    match state
        .store
        .task_page(&sid, limit + 1, q.before_sequence)
        .await
    {
        Ok(mut tasks) => {
            let has_more = tasks.len() > limit as usize;
            tasks.truncate(limit as usize);
            let next = has_more
                .then(|| tasks.last().map(|task| task.sequence_no))
                .flatten();
            let items = tasks
                .iter()
                .map(|task| task_to_json(task, Some(&sid)))
                .collect::<Vec<_>>();
            Json(json!({
                "items": items,
                "has_more": has_more,
                "next_before_sequence": next
            }))
            .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

pub async fn list(
    State(state): State<Arc<AppState>>,
    Query(q): Query<ListQuery>,
) -> impl IntoResponse {
    match q.session_id {
        Some(ref id) => {
            let sid = match SessionId::new(id.clone()) {
                Ok(v) => v,
                Err(e) => {
                    return (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response();
                }
            };
            match state.store.task_list(&sid, None).await {
                Ok(tasks) => {
                    let items: Vec<Value> =
                        tasks.iter().map(|t| task_to_json(t, Some(&sid))).collect();
                    Json(json!(items)).into_response()
                }
                Err(e) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": e.to_string()})),
                )
                    .into_response(),
            }
        }
        None => match state.store.session_list(Default::default()).await {
            Ok(list) => {
                let limit = q.limit.unwrap_or(2000) as usize;
                let mut all_tasks = Vec::new();
                for s in list {
                    if all_tasks.len() >= limit {
                        break;
                    }
                    if let Ok(tasks) = state.store.task_list(&s.id, None).await {
                        all_tasks.extend(tasks.into_iter().map(|t| task_to_json(&t, Some(&s.id))));
                    }
                }
                all_tasks.truncate(limit);
                Json(json!(all_tasks)).into_response()
            }
            Err(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": e.to_string()})),
            )
                .into_response(),
        },
    }
}

pub async fn get(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> impl IntoResponse {
    let tid = TaskId::new(id);
    match state.store.task_info(&tid).await {
        Ok(task) => Json(task_to_full_json(&task)).into_response(),
        Err(e) => (StatusCode::NOT_FOUND, Json(json!({"error": e.to_string()}))).into_response(),
    }
}

pub async fn delete_one(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let tid = TaskId::new(id);
    match state.store.task_delete(&tid).await {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(proxy_store::StoreError::NotFound(_)) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "task not found"})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

pub async fn delete_batch(
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let Some(ids) = body.get("ids").and_then(|v| v.as_array()) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "ids must be an array"})),
        )
            .into_response();
    };
    if ids.len() > 10_000 {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "too many task ids"})),
        )
            .into_response();
    }
    let mut task_ids = Vec::with_capacity(ids.len());
    for id in ids {
        let Some(id) = id.as_str() else {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "every task id must be a string"})),
            )
                .into_response();
        };
        task_ids.push(TaskId::new(id.to_string()));
    }
    match state.store.task_delete_batch(&task_ids).await {
        Ok(deleted) => Json(json!({"ok": true, "deleted": deleted})).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

pub async fn summary(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let tid = TaskId::new(id);
    match state.store.summary_get(&tid).await {
        Ok(s) => Json(json!(s)).into_response(),
        Err(proxy_store::StoreError::NotFound(e)) => {
            (StatusCode::NOT_FOUND, Json(json!({"error": e}))).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// Export full raw task data as a zip archive (in memory).
///
/// `session_ids` are expanded to all their tasks server-side; `ids` are added
/// as-is. Tasks are deduplicated by id. Every entry is `{session_id}-{datetime}.json`.
pub async fn export_tasks(
    State(state): State<Arc<AppState>>,
    Json(body): Json<ExportTasksRequest>,
) -> impl IntoResponse {
    if body.session_ids.is_empty() && body.ids.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "no session_ids or ids provided"})),
        )
            .into_response();
    }

    let mut tasks: Vec<proxy_store::Task> = Vec::new();
    let mut seen: HashSet<TaskId> = HashSet::new();

    for sid in &body.session_ids {
        let Ok(sid) = SessionId::new(sid.clone()) else { continue };
        if let Ok(list) = state.store.task_list_full(&sid).await {
            for task in list {
                if seen.insert(task.id.clone()) {
                    tasks.push(task);
                }
            }
        }
    }

    for id in &body.ids {
        let tid = TaskId::new(id.clone());
        if !seen.insert(tid.clone()) {
            continue;
        }
        if let Ok(task) = state.store.task_info(&tid).await {
            tasks.push(task);
        }
    }

    if tasks.is_empty() {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "no tasks found for export"})),
        )
            .into_response();
    }

    match build_export_zip(&tasks) {
        Ok(bytes) => (
            StatusCode::OK,
            [("content-type", "application/zip")],
            bytes,
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// Build a zip archive of full task JSON, one entry per task.
fn build_export_zip(tasks: &[proxy_store::Task]) -> anyhow::Result<Vec<u8>> {
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    let options = FileOptions::default();
    let mut seen_names: HashSet<String> = HashSet::new();

    for task in tasks {
        let time = format_task_time(task.started_at);
        let mut name = format!("{}-{}.json", task.session_id, time);
        if !seen_names.insert(name.clone()) {
            name = format!("{}-{}-{}.json", task.session_id, time, task.sequence_no);
        }
        let mut json = task_to_full_json(task);
        sanitize_task_response(&mut json);
        let content = serde_json::to_string_pretty(&json)?;
        writer.start_file(name, options)?;
        writer.write_all(content.as_bytes())?;
    }

    Ok(writer.finish()?.into_inner())
}

fn format_task_time(started_at_ms: i64) -> String {
    let Some(dt) = chrono::Local.timestamp_millis_opt(started_at_ms).single() else {
        return "unknown".into();
    };
    dt.format("%Y%m%d-%H%M%S").to_string()
}

/// Convert control characters in the response portion of an exported task.
fn sanitize_task_response(task_json: &mut Value) {
    for key in ["response_body", "normalized_response", "sse_events", "content_text"] {
        if let Some(value) = task_json.get_mut(key) {
            sanitize_json_control_chars(value);
        }
    }
}

fn sanitize_json_control_chars(value: &mut Value) {
    match value {
        Value::String(s) => *s = proxy_common::sanitize_text(s),
        Value::Array(items) => items.iter_mut().for_each(sanitize_json_control_chars),
        Value::Object(map) => map.values_mut().for_each(sanitize_json_control_chars),
        _ => {}
    }
}
