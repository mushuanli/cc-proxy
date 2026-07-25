mod archive;
mod costs;
mod health;
mod requests;
mod sessions;
mod settings;
mod static_files;

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::HeaderMap;
use axum::middleware::{self, Next};
use axum::response::Response;
use serde_json::json;

use crate::AppState;

fn credentials_match(headers: &HeaderMap, token: &str) -> bool {
    let expected = format!("Bearer {}", token);
    let bearer_matches = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|provided| provided == expected);
    let cookie_matches = headers
        .get("cookie")
        .and_then(|v| v.to_str().ok())
        .map(|cookies| {
            cookies
                .split(';')
                .any(|cookie| cookie.trim().strip_prefix("cc_proxy_auth=") == Some(token))
        })
        .unwrap_or(false);
    bearer_matches || cookie_matches
}

/// Log API request method, path, and response status.
async fn api_logger(req: Request, next: Next) -> Response {
    let is_api = req.uri().path().starts_with("/api/");
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let response = next.run(req).await;
    if is_api {
        tracing::info!("[api] {} {} → {}", method, path, response.status());
    }
    response
}

/// Protect control-plane routes with the generated/configured bearer token or
/// the same-site HttpOnly dashboard cookie.
async fn auth_guard(
    state: State<Arc<AppState>>,
    headers: HeaderMap,
    req: Request,
    next: Next,
) -> Response {
    let config = state.config.get().await;
    let is_control = req.uri().path().starts_with("/api/") || req.uri().path() == "/ws";
    if !is_control {
        let is_document = req.method() == axum::http::Method::GET
            && headers
                .get("sec-fetch-dest")
                .and_then(|v| v.to_str().ok())
                .is_none_or(|dest| dest == "document");
        let mut response = next.run(req).await;
        if is_document {
            if let Some(token) = config
                .server
                .auth_token
                .as_deref()
                .filter(|t| !t.is_empty())
            {
                if let Ok(cookie) = axum::http::HeaderValue::from_str(&format!(
                    "cc_proxy_auth={token}; HttpOnly; SameSite=Strict; Path=/"
                )) {
                    response
                        .headers_mut()
                        .append(axum::http::header::SET_COOKIE, cookie);
                }
            }
        }
        return response;
    }
    if let Some(ref token) = config.server.auth_token {
        if token.is_empty() {
            return next.run(req).await;
        }
        if !credentials_match(&headers, token) {
            return Response::builder()
                .status(axum::http::StatusCode::UNAUTHORIZED)
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    json!({"error": "unauthorized"}).to_string(),
                ))
                .unwrap();
        }
    }
    next.run(req).await
}

pub fn build_router(state: Arc<AppState>) -> axum::Router {
    use axum::routing::{get, post, put};

    axum::Router::new()
        // WebSocket
        .route("/ws", get(crate::ws::ws_handler))
        // Sessions
        .route("/api/sessions", get(sessions::list))
        .route(
            "/api/session/:id",
            get(sessions::get)
                .put(sessions::rename)
                .delete(sessions::delete),
        )
        .route("/api/session/:id/export", get(sessions::export_))
        .route("/api/session/:id/summary", get(sessions::summary))
        .route("/api/session/:id/tasks", get(requests::list_session_tasks))
        // Requests (Tasks)
        .route(
            "/api/requests",
            get(requests::list).delete(requests::delete_batch),
        )
        .route(
            "/api/request/:id",
            get(requests::get).delete(requests::delete_one),
        )
        .route("/api/request/:id/summary", get(requests::summary))
        // Settings (config)
        .route(
            "/api/model-pricing",
            get(settings::list_pricing).post(settings::add_pricing),
        )
        .route(
            "/api/model-pricing/:id",
            put(settings::update_pricing).delete(settings::delete_pricing),
        )
        .route(
            "/api/providers",
            get(settings::list_providers).post(settings::add_provider),
        )
        .route(
            "/api/providers/:name",
            put(settings::update_provider).delete(settings::delete_provider),
        )
        .route(
            "/api/upstreams",
            get(settings::list_upstreams).post(settings::add_upstream),
        )
        .route(
            "/api/upstreams/:name",
            put(settings::update_upstream).delete(settings::delete_upstream),
        )
        .route(
            "/api/upstreams/:name/activate",
            post(settings::activate_upstream),
        )
        .route(
            "/api/upstreams/:name/activate-proxy",
            post(settings::activate_proxy_upstream),
        )
        .route(
            "/api/effort",
            get(settings::get_effort).put(settings::set_effort),
        )
        .route(
            "/api/retention",
            get(settings::get_retention).put(settings::update_retention),
        )
        // Capture
        .route("/api/capture", post(settings::toggle_capture))
        .route("/api/capture/status", get(settings::capture_status))
        // Summaries (generate + persist)
        .route("/api/summaries", post(settings::summarize))
        .route("/api/summaries/all", post(settings::summarize_all))
        // Clear
        .route("/api/clear", post(settings::clear_all))
        // Hook (session lifecycle from Claude Code)
        .route("/api/hook-event", post(settings::hook_event))
        // Cleanup
        .route("/api/cleanup", post(settings::trigger_cleanup))
        // Global proxy
        .route(
            "/api/proxy",
            get(settings::get_global_proxy).put(settings::set_global_proxy),
        )
        // Costs
        .route("/api/costs", get(costs::get_costs))
        // Archive
        .route("/api/archive/list", get(archive::list))
        .route("/api/archive/search", get(archive::search))
        .route("/api/archive/file/:name", get(archive::file))
        .route("/api/archive/name/:sid", put(archive::set_name))
        // Health
        .route("/api/health", get(health::check))
        // 404 catch-all: return JSON for /api/*, HTML for everything else
        .fallback(|uri: axum::http::Uri| async move {
            if uri.path().starts_with("/api/") || uri.path() == "/ws" {
                axum::response::Response::builder()
                    .status(axum::http::StatusCode::NOT_FOUND)
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(
                        json!({"error": "not found", "path": uri.path()}).to_string(),
                    ))
                    .unwrap()
            } else {
                static_files::serve(uri).await
            }
        })
        .layer(middleware::from_fn_with_state(state.clone(), auth_guard))
        .layer(middleware::from_fn(api_logger))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::credentials_match;
    use axum::http::{HeaderMap, HeaderValue};

    #[test]
    fn auth_accepts_exact_bearer_or_strict_cookie_only() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "authorization",
            HeaderValue::from_static("Bearer token_1234567890"),
        );
        assert!(credentials_match(&headers, "token_1234567890"));
        assert!(!credentials_match(&headers, "token_123456789"));

        headers.clear();
        headers.insert(
            "cookie",
            HeaderValue::from_static("other=x; cc_proxy_auth=token_1234567890"),
        );
        assert!(credentials_match(&headers, "token_1234567890"));
        assert!(!credentials_match(&headers, "token_123456789"));
    }
}
