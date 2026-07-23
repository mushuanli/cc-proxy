use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use proxy_store::SessionFilter;
use serde_json::json;

use crate::AppState;

/// Add `label` field to session JSON (mirrors `name`, frontend expects `label`).
fn session_to_json(s: &proxy_store::SessionListItem) -> serde_json::Value {
    json!({
        "id": s.id,
        "label": s.name,
        "name": s.name,
        "client_type": s.client_type,
        "client_session_id": s.client_session_id,
        "cwd": s.cwd,
        "project_key": s.project_key,
        "created_at": s.created_at,
        "started_at": s.created_at,           // alias: frontend uses started_at for sort
        "last_activity_at": s.last_activity_at,
        "ended_at": s.last_activity_at,       // alias: frontend uses ended_at
        "task_count": s.task_count,
        "request_count": s.task_count,        // alias: frontend uses request_count
        "total_input_tokens": s.total_input_tokens,
        "total_output_tokens": s.total_output_tokens,
        "total_cache_creation_tokens": s.total_cache_creation_tokens,
        "total_cache_read_tokens": s.total_cache_read_tokens,
        "total_cost_microusd": s.total_cost_microusd,
        "total_cost": s.total_cost_microusd as f64 / 1_000_000.0,
        "archive_dirty": s.archive_dirty,
    })
}

pub async fn list(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let result = state.store.list_sessions(SessionFilter::default()).await;
    match result {
        Ok(sessions) => {
            let count = sessions.len();
            let items: Vec<serde_json::Value> = sessions.iter().map(session_to_json).collect();
            tracing::info!("[api] list sessions: {} sessions", count);
            Json(json!(items)).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

pub async fn get(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> impl IntoResponse {
    let sid = match parse_session_id(id) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let session = state.store.get_session(&sid).await;
    let tasks = state.store.list_tasks(&sid, None).await;

    match (session, tasks) {
        (Ok(Some(s)), Ok(task_list)) => {
            let requests: Vec<serde_json::Value> = task_list
                .iter()
                .map(|t| super::requests::task_to_json(t, Some(&sid)))
                .collect();
            Json(json!({
                "session": {
                    "id": s.id,
                    "label": s.name,
                    "name": s.name,
                    "status": s.status,
                    "client_type": s.client_type,
                    "client_session_id": s.client_session_id,
                    "cwd": s.cwd,
                    "project_key": s.project_key,
                    "created_at": s.created_at,
                    "started_at": s.created_at,
                    "last_activity_at": s.last_activity_at,
                    "ended_at": s.ended_at.unwrap_or(s.last_activity_at),
                    "task_count": s.task_count,
                    "request_count": s.task_count,
                    "completed_task_count": s.completed_task_count,
                    "failed_task_count": s.failed_task_count,
                    "priced_task_count": s.priced_task_count,
                    "total_input_tokens": s.total_input_tokens,
                    "total_output_tokens": s.total_output_tokens,
                    "total_cache_creation_tokens": s.total_cache_creation_tokens,
                    "total_cache_read_tokens": s.total_cache_read_tokens,
                    "total_cost_microusd": s.total_cost_microusd,
                    "total_cost": s.total_cost_microusd as f64 / 1_000_000.0,
                    "total_duration_ms": s.total_duration_ms,
                    "archive_dirty": s.archive_dirty,
                    "latest_provider": s.latest_provider,
                    "latest_model": s.latest_model,
                    "latest_upstream": s.latest_upstream,
                },
                "requests": requests,
            }))
            .into_response()
        }
        (Ok(None), _) => {
            (StatusCode::NOT_FOUND, Json(json!({"error": "not found"}))).into_response()
        }
        (Err(e), _) | (_, Err(e)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

pub async fn rename(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let sid = match parse_session_id(id) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let name = body.get("label").and_then(|v| v.as_str());
    match state.store.name(&sid, name).await {
        Ok(_) => Json(json!({"ok": true})).into_response(),
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

pub async fn delete(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let sid = match parse_session_id(id) {
        Ok(v) => v,
        Err(e) => return e,
    };
    match state.store.delete_session(&sid).await {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(proxy_store::StoreError::NotFound(_)) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "session not found"})),
        )
            .into_response(),
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
    let sid = match parse_session_id(id) {
        Ok(v) => v,
        Err(e) => return e,
    };

    // Get session aggregates (survives task cleanup)
    let session = state.store.get_session(&sid).await;
    let session = match session {
        Ok(Some(s)) => s,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"ok": false, "error": "session not found"})),
            )
                .into_response()
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": e.to_string()})),
            )
                .into_response()
        }
    };

    // Try to get the latest task's detailed summary
    let tasks = state.store.list_tasks(&sid, None).await;
    if let Ok(ref list) = tasks {
        if !list.is_empty() {
            let latest_id = list[0].id.clone();
            if let Ok(s) = state.store.summary(&latest_id).await {
                let mut value = serde_json::to_value(&s).unwrap_or_default();
                if let Some(obj) = value.as_object_mut() {
                    obj.insert("task_input_tokens".into(), json!(s.input_tokens));
                    obj.insert("task_output_tokens".into(), json!(s.output_tokens));
                    obj.insert("input_tokens".into(), json!(session.total_input_tokens));
                    obj.insert("output_tokens".into(), json!(session.total_output_tokens));
                    obj.insert(
                        "cache_creation_tokens".into(),
                        json!(session.total_cache_creation_tokens),
                    );
                    obj.insert(
                        "cache_read_tokens".into(),
                        json!(session.total_cache_read_tokens),
                    );
                    obj.insert("task_count".into(), json!(session.task_count));
                    obj.insert(
                        "completed_task_count".into(),
                        json!(session.completed_task_count),
                    );
                    obj.insert("failed_task_count".into(), json!(session.failed_task_count));
                    obj.insert(
                        "total_cost_microusd".into(),
                        json!(session.total_cost_microusd),
                    );
                    obj.insert("total_duration_ms".into(), json!(session.total_duration_ms));
                }
                return Json(value).into_response();
            }
        }
    }

    // Fallback: return session-level aggregates
    Json(json!({
        "session_id": session.id.as_str(),
        "model": session.latest_model.as_deref().unwrap_or(""),
        "started_at": session.created_at,
        "input_tokens": session.total_input_tokens,
        "output_tokens": session.total_output_tokens,
        "cache_creation_tokens": session.total_cache_creation_tokens,
        "cache_read_tokens": session.total_cache_read_tokens,
        "status_code": null,
        "stop_reason": null,
        "user_prompts": [],
        "assistant_actions": [],
        "touched_files": [],
        "final_response": "",
        "stats": {
            "total_messages": 0,
            "user_prompt_count": 0,
            "tool_result_count": 0,
            "tool_call_count": 0,
            "tool_call_by_name": {},
            "thinking_block_count": 0,
        },
        "task_count": session.task_count,
        "completed_task_count": session.completed_task_count,
        "failed_task_count": session.failed_task_count,
        "total_cost_microusd": session.total_cost_microusd,
        "total_duration_ms": session.total_duration_ms,
    }))
    .into_response()
}

pub async fn export_(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    let sid = match parse_session_id(id) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let tasks = state.store.list_tasks(&sid, None).await;
    let format = q.get("format").map(|s| s.as_str()).unwrap_or("json");

    match tasks {
        Ok(list) => match format {
            "json" => Json(json!(list)).into_response(),
            "yaml" | "yml" => match serde_yaml::to_string(&list) {
                Ok(y) => {
                    (StatusCode::OK, [("content-type", "application/x-yaml")], y).into_response()
                }
                Err(e) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": e.to_string()})),
                )
                    .into_response(),
            },
            _ => (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": format!("unsupported format: {}", format)})),
            )
                .into_response(),
        },
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// Parse a SessionId from a path parameter, returning a 400 error on invalid input.
#[allow(clippy::result_large_err)]
fn parse_session_id(id: String) -> Result<proxy_common::SessionId, axum::response::Response> {
    proxy_common::SessionId::new(id).map_err(|e| {
        axum::response::Response::builder()
            .status(axum::http::StatusCode::BAD_REQUEST)
            .header("content-type", "application/json")
            .body(axum::body::Body::from(json!({"error": e}).to_string()))
            .unwrap()
    })
}
