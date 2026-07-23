use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use proxy_common::{SessionId, TaskId};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::AppState;

#[derive(Deserialize, Default)]
pub struct ListQuery {
    pub session_id: Option<String>,
    pub limit: Option<u32>,
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
        "cost_microusd": task.cost_microusd,
        "cost": task.cost_microusd as f64 / 1_000_000.0,
        "priced": task.priced,
        "duration_ms": task.duration_ms,
        "time_to_first_token_ms": task.ttft_ms,
        "ttft_ms": task.ttft_ms,
        "last_msg_summary": task.summary_json,
        "messages_count": task.messages_count,
    })
}

/// Transform full Task to ProxiedRequest-compatible JSON for detail view.
fn task_to_full_json(task: &proxy_store::Task) -> Value {
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
        "last_msg_summary": task.summary_json
    })
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
                    return Json(json!({"error": e})).into_response();
                }
            };
            match state.store.list_tasks(&sid, None) {
                Ok(tasks) => {
                    let items: Vec<Value> =
                        tasks.iter().map(|t| task_to_json(t, Some(&sid))).collect();
                    Json(json!(items)).into_response()
                }
                Err(e) => Json(json!({"error": e.to_string()})).into_response(),
            }
        }
        None => {
            let sessions = state.store.list_sessions(Default::default());
            match sessions {
                Ok(list) => {
                    let limit = q.limit.unwrap_or(2000) as usize;
                    let mut all_tasks = Vec::new();
                    for s in list {
                        if all_tasks.len() >= limit {
                            break;
                        }
                        if let Ok(tasks) = state.store.list_tasks(&s.id, None) {
                            all_tasks
                                .extend(tasks.into_iter().map(|t| task_to_json(&t, Some(&s.id))));
                        }
                    }
                    all_tasks.truncate(limit);
                    Json(json!(all_tasks)).into_response()
                }
                Err(e) => Json(json!({"error": e.to_string()})).into_response(),
            }
        }
    }
}

pub async fn get(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> impl IntoResponse {
    let tid = TaskId::new(id);
    match state.store.info(&tid) {
        Ok(task) => Json(task_to_full_json(&task)).into_response(),
        Err(e) => Json(json!({"error": e.to_string()})).into_response(),
    }
}

pub async fn delete_one(Path(_id): Path<String>) -> impl IntoResponse {
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(json!({"error": "not implemented"})),
    )
}

pub async fn delete_batch(Json(_body): Json<serde_json::Value>) -> impl IntoResponse {
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(json!({"error": "not implemented"})),
    )
}

pub async fn summary(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let tid = TaskId::new(id);
    match state.store.summary(&tid) {
        Ok(s) => Json(json!(s)).into_response(),
        Err(e) => Json(json!({"error": e.to_string()})).into_response(),
    }
}
