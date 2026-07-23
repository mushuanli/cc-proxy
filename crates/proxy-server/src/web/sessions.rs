use std::sync::Arc;

use axum::extract::{Path, Query, State};
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

pub async fn list(
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let result = state.store.list_sessions(SessionFilter::default());
    match result {
        Ok(sessions) => {
            let count = sessions.len();
            let items: Vec<serde_json::Value> = sessions.iter().map(session_to_json).collect();
            tracing::info!("[api] list sessions: {} sessions", count);
            Json(json!(items)).into_response()
        }
        Err(e) => Json(json!({"error": e.to_string()})).into_response(),
    }
}

pub async fn get(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let sid = match parse_session_id(id) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let session = state.store.list_sessions(SessionFilter {
        id_or_name: Some(sid.as_str().to_string()),
        ..Default::default()
    });
    let tasks = state.store.list_tasks(&sid, None);

    match (session, tasks) {
        (Ok(mut sessions), Ok(task_list)) => {
            if sessions.is_empty() {
                return Json(json!({"error": "not found"})).into_response();
            }
            let requests: Vec<serde_json::Value> = task_list
                .iter()
                .map(|t| super::requests::task_to_json(t, Some(&sid)))
                .collect();
            Json(json!({
                "session": session_to_json(&sessions.remove(0)),
                "requests": requests,
            }))
            .into_response()
        }
        (Err(e), _) | (_, Err(e)) => Json(json!({"error": e.to_string()})).into_response(),
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
    match state.store.name(&sid, name) {
        Ok(_) => Json(json!({"ok": true})).into_response(),
        Err(e) => Json(json!({"error": e.to_string()})).into_response(),
    }
}

pub async fn delete(
    State(_state): State<Arc<AppState>>,
    Path(_id): Path<String>,
) -> impl IntoResponse {
    Json(json!({"ok": false, "error": "not implemented"})).into_response()
}

pub async fn summary(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let sid = match parse_session_id(id) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let tasks = state.store.list_tasks(&sid, None);

    match tasks {
        Ok(list) if !list.is_empty() => {
            // Return the summary of the latest task as a session-level view
            let latest_id = list[0].id.clone();
            match state.store.summary(&latest_id) {
                Ok(s) => Json(json!(s)).into_response(),
                Err(_) => {
                    // Tasks exist but can't be summarized — return basic stats
                    let total_in: u64 = list.iter().map(|t| t.input_tokens).sum();
                    let total_out: u64 = list.iter().map(|t| t.output_tokens).sum();
                    let models: Vec<&str> = list.iter().map(|t| t.resolved_model.as_str()).collect();
                    Json(json!({
                        "model": models.first().unwrap_or(&""),
                        "started_at": list[0].started_at,
                        "input_tokens": total_in,
                        "output_tokens": total_out,
                        "cache_read_tokens": 0,
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
                    })).into_response()
                }
            }
        }
        _ => {
            // No tasks or error — return session metadata as a fallback
            let sessions = state.store.list_sessions(SessionFilter {
                id_or_name: Some(sid.as_str().to_string()),
                ..Default::default()
            });
            match sessions {
                Ok(sessions) if !sessions.is_empty() => {
                    let s = &sessions[0];
                    Json(json!({
                        "model": "",
                        "started_at": s.created_at,
                        "input_tokens": 0,
                        "output_tokens": 0,
                        "cache_read_tokens": 0,
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
                    })).into_response()
                }
                _ => Json(json!({"ok": false, "error": "session not found"})).into_response(),
            }
        }
    }
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
    let tasks = state.store.list_tasks(&sid, None);
    let format = q.get("format").map(|s| s.as_str()).unwrap_or("json");

    match tasks {
        Ok(list) => {
            if format == "json" {
                Json(json!(list)).into_response()
            } else {
                Json(json!({"error": format!("unsupported format: {}", format)})).into_response()
            }
        }
        Err(e) => Json(json!({"error": e.to_string()})).into_response(),
    }
}

/// Parse a SessionId from a path parameter, returning a 400 error on invalid input.
fn parse_session_id(id: String) -> Result<proxy_common::SessionId, axum::response::Response> {
    proxy_common::SessionId::new(id).map_err(|e| {
        axum::response::Response::builder()
            .status(axum::http::StatusCode::BAD_REQUEST)
            .header("content-type", "application/json")
            .body(axum::body::Body::from(
                json!({"error": e}).to_string(),
            ))
            .unwrap()
    })
}
