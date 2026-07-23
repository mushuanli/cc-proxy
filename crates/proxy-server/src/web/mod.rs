mod archive;
mod costs;
mod health;
mod requests;
mod sessions;
mod settings;
mod static_files;

use std::sync::Arc;

use axum::extract::Request;
use axum::middleware::{self, Next};
use axum::response::Response;
use serde_json::json;

use crate::AppState;

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
        // MCP destination
        .route(
            "/api/mcp-destination",
            get(settings::get_mcp).put(settings::set_mcp),
        )
        // Clear
        .route("/api/clear", post(settings::clear_all))
        .route("/api/clear-mcp", post(settings::clear_mcp))
        .route("/api/clear-hooks", post(settings::clear_hooks))
        // Flush
        .route("/api/flush", post(settings::flush))
        .route("/api/flush-all", post(settings::flush_all))
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
        .layer(middleware::from_fn(api_logger))
        .with_state(state)
}
