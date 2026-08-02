use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use proxy_common::{ConfigStore, ProviderInfo, UpstreamInfo, WsMessage};
use serde_json::json;

use crate::AppState;

/// Map an error message to the best HTTP status code.
fn err_response(msg: &str) -> axum::response::Response {
    let code = if msg.contains("not found") || msg.contains("NotFound") {
        StatusCode::NOT_FOUND
    } else if msg.contains("validation") || msg.contains("invalid") || msg.contains("Validation") {
        StatusCode::BAD_REQUEST
    } else if msg.contains("duplicate") || msg.contains("conflict") {
        StatusCode::CONFLICT
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    };
    (code, Json(json!({"error": msg}))).into_response()
}

// ── Model Pricing ──

pub async fn list_pricing(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let config = state.config.get().await;
    let count = config.model_pricing.len();
    tracing::info!("[api] list_pricing: {} entries", count);
    Json(json!(config.model_pricing)).into_response()
}

pub async fn add_pricing(
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let result = state
        .config
        .update(move |c| {
            let mp: proxy_common::ModelPricing = serde_json::from_value(body)
                .map_err(|e| proxy_common::ConfigError::Validation(e.to_string()))?;
            c.model_pricing.push(mp);
            Ok(())
        })
        .await;
    match result {
        Ok(config) => {
            state.events.publish(upstream_changed(&state.config).await);
            let id = config
                .model_pricing
                .last()
                .map(|p| p.id.as_str())
                .unwrap_or("?");
            tracing::info!("[api] add_pricing: id={}", id);
            Json(json!({"ok": true, "model_pricing": config.model_pricing})).into_response()
        }
        Err(e) => {
            tracing::warn!("[api] add_pricing failed: {}", e);
            err_response(&e.to_string())
        }
    }
}

pub async fn update_pricing(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let log_id = id.clone();
    let result = state
        .config
        .update(move |c| {
            let mp: proxy_common::ModelPricing = serde_json::from_value(body)
                .map_err(|e| proxy_common::ConfigError::Validation(e.to_string()))?;
            c.model_pricing.retain(|p| p.id != id);
            c.model_pricing.push(mp);
            Ok(())
        })
        .await;
    match result {
        Ok(_) => {
            state.events.publish(upstream_changed(&state.config).await);
            tracing::info!("[api] update_pricing: id={}", log_id);
            Json(json!({"ok": true})).into_response()
        }
        Err(e) => {
            tracing::warn!("[api] update_pricing failed: {}", e);
            err_response(&e.to_string())
        }
    }
}

pub async fn delete_pricing(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let log_id = id.clone();
    let result = state
        .config
        .update(move |c| {
            c.model_pricing.retain(|p| p.id != id);
            Ok(())
        })
        .await;
    match result {
        Ok(_) => {
            state.events.publish(upstream_changed(&state.config).await);
            tracing::info!("[api] delete_pricing: id={}", log_id);
            Json(json!({"ok": true})).into_response()
        }
        Err(e) => {
            tracing::warn!("[api] delete_pricing failed: {}", e);
            err_response(&e.to_string())
        }
    }
}

// ── Providers ──

pub async fn list_providers(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let config = state.config.get().await;
    let count = config.proxy.providers.len();
    tracing::info!("[api] list_providers: {} entries", count);
    let infos: Vec<serde_json::Value> = config
        .proxy
        .providers
        .iter()
        .map(|p| {
            json!({"name": p.name, "url": p.url, "has_token": p.token.is_some(), "proxy": p.proxy})
        })
        .collect();
    Json(json!(infos)).into_response()
}

pub async fn add_provider(
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let name = body
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let url = body.get("url").and_then(|v| v.as_str()).unwrap_or("");
    let token = body.get("token").and_then(|v| v.as_str()).map(String::from);
    let provider_proxy = body.get("proxy").and_then(|v| v.as_str()).map(String::from);
    let log_name = name.clone();
    let result = state
        .config
        .update(move |c| {
            c.proxy.providers.push(proxy_common::Provider {
                name,
                url: url.into(),
                token,
                proxy: provider_proxy,
            });
            Ok(())
        })
        .await;
    match result {
        Ok(_) => {
            state.events.publish(upstream_changed(&state.config).await);
            tracing::info!("[api] add_provider: name={}", log_name);
            Json(json!({"ok": true})).into_response()
        }
        Err(e) => {
            tracing::warn!("[api] add_provider failed: {}", e);
            err_response(&e.to_string())
        }
    }
}

pub async fn update_provider(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let log_name = name.clone();
    let result = state
        .config
        .update(move |c| {
            if let Some(p) = c.proxy.providers.iter_mut().find(|p| p.name == name) {
                if let Some(url) = body.get("url").and_then(|v| v.as_str()) {
                    p.url = url.into();
                }
                if body.get("token").is_some() {
                    p.token = body.get("token").and_then(|v| v.as_str()).map(String::from);
                }
                if body.get("proxy").is_some() {
                    p.proxy = body.get("proxy").and_then(|v| v.as_str()).map(|s| {
                        if s.is_empty() {
                            String::new()
                        } else {
                            s.to_string()
                        }
                    });
                }
            }
            Ok(())
        })
        .await;
    match result {
        Ok(_) => {
            state.events.publish(upstream_changed(&state.config).await);
            tracing::info!("[api] update_provider: name={}", log_name);
            Json(json!({"ok": true})).into_response()
        }
        Err(e) => {
            tracing::warn!("[api] update_provider failed: {}", e);
            err_response(&e.to_string())
        }
    }
}

pub async fn delete_provider(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let log_name = name.clone();
    let result = state
        .config
        .update(move |c| {
            c.proxy.providers.retain(|p| p.name != name);
            Ok(())
        })
        .await;
    match result {
        Ok(_) => {
            state.events.publish(upstream_changed(&state.config).await);
            tracing::info!("[api] delete_provider: name={}", log_name);
            Json(json!({"ok": true})).into_response()
        }
        Err(e) => {
            tracing::warn!("[api] delete_provider failed: {}", e);
            err_response(&e.to_string())
        }
    }
}

// ── Upstreams ──

pub async fn list_upstreams(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let config = state.config.get().await;
    let count = config.proxy.upstreams.len();
    tracing::info!(
        "[api] list_upstreams: {} upstreams, active={}",
        count,
        config.proxy.active_upstream
    );
    let active = &config.proxy.active_upstream;
    Json(json!({
        "active_upstream": config.proxy.active_upstream,
        "active_proxy_upstream": config.proxy.active_proxy_upstream,
        "active_effort": config.proxy.active_effort,
        "http_proxy": config.proxy.http_proxy,
        "upstreams": config.proxy.upstreams.iter().map(|u| {
            json!({
                "name": u.name,
                "active": u.name == *active,
                "proxy_active": u.name == config.proxy.active_proxy_upstream,
                "high": u.high,
                "mid": u.mid,
                "low": u.low,
                "default": u.default,
                "effort": u.effort,
            })
        }).collect::<Vec<_>>(),
        "providers": config.proxy.providers.iter().map(|p| json!({"name": p.name, "url": p.url, "has_token": p.token.is_some(), "proxy": p.proxy})).collect::<Vec<_>>(),
        "model_pricing": config.model_pricing,
    })).into_response()
}

pub async fn add_upstream(
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let name = body
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("?")
        .to_string();
    let result = state
        .config
        .update(move |c| {
            let u: proxy_common::UpstreamConfig = serde_json::from_value(body)
                .map_err(|e| proxy_common::ConfigError::Validation(e.to_string()))?;
            c.proxy.upstreams.push(u);
            Ok(())
        })
        .await;
    match result {
        Ok(_) => {
            state.events.publish(upstream_changed(&state.config).await);
            tracing::info!("[api] add_upstream: name={}", name);
            Json(json!({"ok": true})).into_response()
        }
        Err(e) => {
            tracing::warn!("[api] add_upstream failed: {}", e);
            err_response(&e.to_string())
        }
    }
}

pub async fn update_upstream(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let log_name = name.clone();
    let result = state
        .config
        .update(move |c| {
            let u: proxy_common::UpstreamConfig = serde_json::from_value(body)
                .map_err(|e| proxy_common::ConfigError::Validation(e.to_string()))?;
            c.proxy.upstreams.retain(|x| x.name != name);
            c.proxy.upstreams.push(u);
            Ok(())
        })
        .await;
    match result {
        Ok(_) => {
            state.events.publish(upstream_changed(&state.config).await);
            tracing::info!("[api] update_upstream: name={}", log_name);
            Json(json!({"ok": true})).into_response()
        }
        Err(e) => {
            tracing::warn!("[api] update_upstream failed: {}", e);
            err_response(&e.to_string())
        }
    }
}

pub async fn delete_upstream(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let log_name = name.clone();
    let result = state
        .config
        .update(move |c| {
            if c.proxy.upstreams.len() <= 1 {
                return Err(proxy_common::ConfigError::Validation(
                    "cannot delete last upstream".into(),
                ));
            }
            let was_active = c.proxy.active_upstream == name;
            let was_proxy_active = c.proxy.active_proxy_upstream == name;
            c.proxy.upstreams.retain(|u| u.name != name);
            if was_active {
                c.proxy.active_upstream = c
                    .proxy
                    .upstreams
                    .first()
                    .map(|u| u.name.clone())
                    .unwrap_or_default();
            }
            if was_proxy_active {
                c.proxy.active_proxy_upstream = c
                    .proxy
                    .upstreams
                    .first()
                    .map(|u| u.name.clone())
                    .unwrap_or_default();
            }
            Ok(())
        })
        .await;
    match result {
        Ok(_) => {
            state.events.publish(upstream_changed(&state.config).await);
            tracing::info!("[api] delete_upstream: name={}", log_name);
            Json(json!({"ok": true})).into_response()
        }
        Err(e) => {
            tracing::warn!("[api] delete_upstream failed: {}", e);
            err_response(&e.to_string())
        }
    }
}

pub async fn activate_upstream(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let log_name = name.clone();
    let result = state
        .config
        .update(move |c| {
            c.proxy.active_upstream = name;
            Ok(())
        })
        .await;
    match result {
        Ok(_) => {
            state.events.publish(upstream_changed(&state.config).await);
            tracing::info!("[api] activate_upstream: {}", log_name);
            Json(json!({"ok": true})).into_response()
        }
        Err(e) => {
            tracing::warn!("[api] activate_upstream failed: {}", e);
            err_response(&e.to_string())
        }
    }
}

pub async fn activate_proxy_upstream(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let log_name = name.clone();
    let result = state
        .config
        .update(move |c| {
            c.proxy.active_proxy_upstream = name;
            Ok(())
        })
        .await;
    match result {
        Ok(_) => {
            state.events.publish(upstream_changed(&state.config).await);
            tracing::info!("[api] activate proxy upstream: {}", log_name);
            Json(json!({"ok": true})).into_response()
        }
        Err(e) => err_response(&e.to_string()),
    }
}

// ── Effort ──

pub async fn get_effort(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let config = state.config.get().await;
    tracing::info!("[api] get_effort: effort={}", config.proxy.active_effort);
    Json(json!({"effort": config.proxy.active_effort})).into_response()
}

pub async fn set_effort(
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let effort = body
        .get("effort")
        .and_then(|v| v.as_str())
        .unwrap_or("auto")
        .to_string();
    let valid = ["auto", "low", "medium", "high", "xhigh", "max", "ultracode"];
    if !valid.contains(&effort.as_str()) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("invalid effort: {}", effort)})),
        )
            .into_response();
    }
    let log_effort = effort.clone();
    let result = state
        .config
        .update(move |c| {
            c.proxy.active_effort = effort;
            Ok(())
        })
        .await;
    match result {
        Ok(_) => {
            state.events.publish(upstream_changed(&state.config).await);
            tracing::info!("[api] set_effort: {}", log_effort);
            Json(json!({"ok": true})).into_response()
        }
        Err(e) => {
            tracing::warn!("[api] set_effort failed: {}", e);
            err_response(&e.to_string())
        }
    }
}

// ── Retention ──

pub async fn get_retention(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let config = state.config.get().await;
    tracing::info!(
        "[api] get_retention: retention_hours={}, max_sessions={}, delete_after_days={}",
        config.proxy.request_retention_hours,
        config.proxy.session_max_count,
        config.proxy.session_delete_after_days,
    );
    Json(json!({
        "request_retention_hours": config.proxy.request_retention_hours,
        "session_max_count": config.proxy.session_max_count,
        "session_delete_after_days": config.proxy.session_delete_after_days,
    }))
    .into_response()
}

pub async fn update_retention(
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let result = state
        .config
        .update(move |c| {
            if let Some(v) = body.get("request_retention_hours").and_then(|v| v.as_u64()) {
                c.proxy.request_retention_hours = v as u32;
            }
            if let Some(v) = body.get("session_max_count").and_then(|v| v.as_u64()) {
                c.proxy.session_max_count = v as u32;
            }
            if let Some(v) = body
                .get("session_delete_after_days")
                .and_then(|v| v.as_u64())
            {
                c.proxy.session_delete_after_days = v as u32;
            }
            Ok(())
        })
        .await;
    match result {
        Ok(_) => {
            tracing::info!("[api] update_retention: updated");
            Json(json!({"ok": true})).into_response()
        }
        Err(e) => {
            tracing::warn!("[api] update_retention failed: {}", e);
            err_response(&e.to_string())
        }
    }
}

// ── Capture ──

pub async fn toggle_capture(
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let enabled = body
        .get("enabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    state.capture.set_enabled(enabled);
    tracing::info!("[api] toggle_capture: enabled={}", enabled);
    Json(json!({"ok": true, "enabled": enabled})).into_response()
}

pub async fn capture_status(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let enabled = state.capture.is_enabled();
    tracing::info!("[api] capture_status: enabled={}", enabled);
    Json(json!({"enabled": enabled})).into_response()
}

// ── Global proxy ──

pub async fn get_global_proxy(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let config = state.config.get().await;
    Json(json!({"http_proxy": config.proxy.http_proxy})).into_response()
}

pub async fn set_global_proxy(
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let proxy_val = body
        .get("http_proxy")
        .and_then(|v| v.as_str())
        .and_then(|s| {
            if s.is_empty() {
                None
            } else {
                Some(s.to_string())
            }
        });

    let result = state
        .config
        .update(move |c| {
            c.proxy.http_proxy = proxy_val;
            Ok(())
        })
        .await;

    match result {
        Ok(_) => {
            state.events.publish(upstream_changed(&state.config).await);
            Json(json!({"ok": true})).into_response()
        }
        Err(e) => err_response(&e.to_string()),
    }
}

// ── Clear ──

pub async fn clear_all(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    state.events.publish(WsMessage::Cleared);
    tracing::info!("[api] clear_all");
    Json(json!({"ok": true})).into_response()
}

// ── Hook (session lifecycle signals from proxy-hook-agent) ──

pub async fn hook_event(
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let event_name = body
        .get("hook_event_name")
        .or_else(|| body.get("hookEventName"))
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    if matches!(event_name, "Stop" | "SessionEnd") {
        if let Some(raw_sid) = body
            .get("session_id")
            .or_else(|| body.get("sessionId"))
            .and_then(|v| v.as_str())
        {
            if let Ok(sid) = proxy_common::SessionId::new(raw_sid.to_string()) {
                if let Err(e) = state
                    .store
                    .session_stop(&sid, chrono::Utc::now().timestamp_millis())
                    .await
                {
                    tracing::warn!("[api] failed to stop session {}: {}", sid, e);
                }
            }
        }
    }

    // Ingest hook observations for session timeline correlation.
    if let Some(raw_sid) = body
        .get("session_id")
        .or_else(|| body.get("sessionId"))
        .and_then(|v| v.as_str())
    {
        let parser = proxy_session::HookParser::default();
        let hook_input = body.get("hook_input").unwrap_or(&body);
        let observations = parser.parse_hook_event(raw_sid, event_name, &body, hook_input);
        for obs in observations {
            if let Err(e) = state.session.record_observation(&obs) {
                tracing::warn!("[api] failed to record hook observation: {}", e);
            }
        }
    }
    Json(json!({"ok": true})).into_response()
}

// ── Persisted summaries ──

pub async fn summarize(
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let session_ids = match parse_summary_session_ids(&body) {
        Ok(ids) => ids,
        Err(response) => return response,
    };
    generate_summaries(&state, Some(&session_ids)).await
}

pub async fn summarize_all(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    generate_summaries(&state, None).await
}

fn parse_summary_session_ids(
    body: &serde_json::Value,
) -> Result<Vec<proxy_common::SessionId>, axum::response::Response> {
    let values = body
        .get("session_ids")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| bad_request("expected session_ids array"))?;
    let ids = values
        .iter()
        .map(parse_summary_session_id)
        .collect::<Result<Vec<_>, _>>()?;
    if ids.is_empty() {
        return Err(bad_request("no valid session_ids"));
    }
    Ok(ids)
}

fn parse_summary_session_id(
    value: &serde_json::Value,
) -> Result<proxy_common::SessionId, axum::response::Response> {
    let raw = value
        .as_str()
        .ok_or_else(|| bad_request("invalid session_id type"))?;
    proxy_common::SessionId::new(raw.to_string()).map_err(|error| bad_request(&error))
}

fn bad_request(error: &str) -> axum::response::Response {
    (StatusCode::BAD_REQUEST, Json(json!({"error": error}))).into_response()
}

async fn generate_summaries(
    state: &Arc<AppState>,
    session_ids: Option<&[proxy_common::SessionId]>,
) -> axum::response::Response {
    let config = state.config.get().await;
    let options = proxy_store::ArchiveOptions {
        task_retention_hours: config.proxy.request_retention_hours,
        force: true,
    };
    match state.store.archive_create(session_ids, options).await {
        Ok(results) => {
            let summarized: Vec<String> = results
                .iter()
                .map(|a| a.session_id.as_str().to_string())
                .collect();
            tracing::info!("[api] generated {} session summaries", summarized.len());
            Json(json!({"summarized": summarized, "errors": []})).into_response()
        }
        Err(e) => {
            tracing::error!("[api] summary generation failed: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": e.to_string()})),
            )
                .into_response()
        }
    }
}

// ── Cleanup ──

/// Trigger cleanup of old tasks past retention for all archived sessions.
pub async fn trigger_cleanup(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let config = state.config.get().await;
    let retention_hours = config.proxy.request_retention_hours;
    let delete_after_days = config.proxy.session_delete_after_days;
    let max_sessions = config.proxy.session_max_count;
    if retention_hours == 0 && delete_after_days == 0 && max_sessions == 0 {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "all retention policies are disabled"})),
        )
            .into_response();
    }

    let deleted_requests = if retention_hours > 0 {
        match state.store.cleanup_tasks(retention_hours as u64).await {
            Ok(deleted) => deleted,
            Err(e) => {
                tracing::error!("[api] task cleanup failed: {}", e);
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": e.to_string()})),
                )
                    .into_response();
            }
        }
    } else {
        0
    };
    match state
        .store
        .cleanup_sessions(delete_after_days as u64, max_sessions as u64)
        .await
    {
        Ok(deleted_sessions) => {
            tracing::info!(
                "[api] cleanup: {} tasks, {} sessions deleted",
                deleted_requests,
                deleted_sessions
            );
            Json(json!({
                "ok": true,
                "deleted": deleted_requests,
                "deleted_requests": deleted_requests,
                "deleted_sessions": deleted_sessions
            }))
            .into_response()
        }
        Err(e) => {
            tracing::error!("[api] cleanup failed: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": e.to_string()})),
            )
                .into_response()
        }
    }
}

// ── Helper ──

async fn upstream_changed(config: &ConfigStore) -> WsMessage {
    let c = config.get().await;
    let active = &c.proxy.active_upstream;
    WsMessage::UpstreamChanged {
        active_upstream: active.clone(),
        active_proxy_upstream: c.proxy.active_proxy_upstream.clone(),
        upstreams: c
            .proxy
            .upstreams
            .iter()
            .map(|u| UpstreamInfo {
                name: u.name.clone(),
                active: u.name == *active,
                proxy_active: u.name == c.proxy.active_proxy_upstream,
                high: u.high.as_ref().map(|t| t.into()),
                mid: u.mid.as_ref().map(|t| t.into()),
                low: u.low.as_ref().map(|t| t.into()),
                default: u.default.as_ref().map(|t| t.into()),
                effort: u.effort.clone(),
            })
            .collect(),
        providers: c
            .proxy
            .providers
            .iter()
            .map(|p| ProviderInfo {
                name: p.name.clone(),
                url: p.url.clone(),
                has_token: p.token.is_some(),
                proxy: p.proxy.clone(),
            })
            .collect(),
        active_effort: c.proxy.active_effort.clone(),
        model_pricing: c.model_pricing.clone(),
        http_proxy: c.proxy.http_proxy.clone(),
    }
}
