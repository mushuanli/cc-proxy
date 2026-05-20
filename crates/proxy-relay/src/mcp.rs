use std::sync::Arc;

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use bytes::Bytes;
use proxy_common::EventBus;
use proxy_common::WsMessage;
use proxy_store::ProxyStore;
use serde_json::json;
use tokio::sync::RwLock;

/// MCP JSON-RPC proxy relay.
///
/// Forwards MCP requests to the configured destination URL.
/// Mounted on port :9999.
#[derive(Clone)]
pub struct McpRelay {
    #[allow(dead_code)]
    store: ProxyStore,
    events: EventBus,
    http_client: reqwest::Client,
    destination: Arc<RwLock<Option<String>>>,
}

impl McpRelay {
    pub fn new(
        store: ProxyStore,
        events: EventBus,
        client: reqwest::Client,
    ) -> Self {
        Self {
            store,
            events,
            http_client: client,
            destination: Arc::new(RwLock::new(None)),
        }
    }

    /// Return an axum Router for MCP proxying (mount on :9999).
    pub fn build_router(self) -> axum::Router {
        axum::Router::new()
            .fallback(mcp_handler)
            .with_state(self)
    }

    /// Set the MCP forwarding destination.
    pub async fn set_destination(&self, url: Option<String>) {
        let destination_url = url.clone();
        *self.destination.write().await = url;
        let msg = WsMessage::McpConfigChanged {
            destination_url,
        };
        self.events.publish(msg);
    }

    /// Get the current MCP forwarding destination.
    pub async fn get_destination(&self) -> Option<String> {
        self.destination.read().await.clone()
    }
}

async fn mcp_handler(
    State(relay): State<McpRelay>,
    headers: HeaderMap,
    body: Bytes,
) -> Response<Body> {
    let dest = relay.destination.read().await;
    let destination = match dest.as_ref() {
        Some(d) => d.clone(),
        None => {
            drop(dest);
            return mcp_not_configured().into_response();
        }
    };
    drop(dest);

    let method_str = if let Ok(json) = serde_json::from_slice::<serde_json::Value>(&body) {
        json.get("method")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string()
    } else {
        "parse_error".to_string()
    };

    let upstream_url = format!("{}/", destination.trim_end_matches('/'));
    let upstream_req = match relay
        .http_client
        .post(&upstream_url)
        .headers(filter_mcp_headers(&headers))
        .body(body)
        .build()
    {
        Ok(req) => req,
        Err(e) => {
            let msg = WsMessage::NewMcp(proxy_common::models::ProxiedRequest {
                error: Some(format!("Failed to build MCP request: {}", e)),
                ..Default::default()
            });
            relay.events.publish(msg);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "jsonrpc": "2.0",
                    "error": {"code": -32603, "message": e.to_string()},
                    "id": null
                })),
            )
                .into_response();
        }
    };

    match relay.http_client.execute(upstream_req).await {
        Ok(resp) => {
            let status = resp.status();
            let resp_headers = resp.headers().clone();

            match resp.bytes().await {
                Ok(resp_bytes) => {
                    let msg = WsMessage::NewMcp(proxy_common::models::ProxiedRequest {
                        status_code: Some(status.as_u16()),
                        response_body: Some(String::from_utf8_lossy(&resp_bytes).to_string()),
                        model: Some(method_str),
                        ..Default::default()
                    });
                    relay.events.publish(msg);

                    let mut response = Response::builder().status(status);
                    for (k, v) in resp_headers.iter() {
                        if k.as_str().to_lowercase() != "transfer-encoding" {
                            response = response.header(k.clone(), v.clone());
                        }
                    }
                    response.body(Body::from(resp_bytes)).unwrap()
                }
                Err(e) => {
                    let msg = WsMessage::NewMcp(proxy_common::models::ProxiedRequest {
                        error: Some(format!("Failed to read MCP response: {}", e)),
                        ..Default::default()
                    });
                    relay.events.publish(msg);
                    (
                        StatusCode::BAD_GATEWAY,
                        Json(json!({
                            "jsonrpc": "2.0",
                            "error": {"code": -32603, "message": e.to_string()},
                            "id": null
                        })),
                    )
                        .into_response()
                }
            }
        }
        Err(e) => {
            let msg = WsMessage::NewMcp(proxy_common::models::ProxiedRequest {
                error: Some(format!("MCP upstream error: {}", e)),
                ..Default::default()
            });
            relay.events.publish(msg);
            (
                StatusCode::BAD_GATEWAY,
                Json(json!({
                    "jsonrpc": "2.0",
                    "error": {"code": -32603, "message": format!("Upstream error: {}", e)},
                    "id": null
                })),
            )
                .into_response()
        }
    }
}

fn mcp_not_configured() -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::OK,
        Json(json!({
            "jsonrpc": "2.0",
            "error": {
                "code": -32603,
                "message": "MCP proxy destination not configured. Set it at http://localhost:5000"
            },
            "id": null
        })),
    )
}

fn filter_mcp_headers(headers: &HeaderMap) -> HeaderMap {
    let mut fwd = HeaderMap::new();
    for (k, v) in headers.iter() {
        let key = k.as_str().to_lowercase();
        if key == "host"
            || key == "connection"
            || key == "transfer-encoding"
            || key == "accept-encoding"
        {
            continue;
        }
        fwd.insert(k.clone(), v.clone());
    }
    fwd
}
