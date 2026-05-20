//! RelayHandler — proxy entry point.
//!
//! Handles three proxy modes:
//! 1. CONNECT tunnel (HTTPS forward proxy)
//! 2. Forward proxy (absolute URI in request line)
//! 3. Reverse proxy (path-based, e.g. /v1/messages)
//!
//! Flow: resolve route → dispatch upstream → store.write() → events.publish()

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::Response;
use bytes::Bytes;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use proxy_common::{ConfigStore, EventBus};
use proxy_common::{ClientType, SessionId, TaskStatus, TaskUsage, WsMessage};
use proxy_store::{NewSessionDefaults, NewTask, ProxyStore};

use crate::upstream;

/// Proxy relay handler. Owns the config, store, events, and HTTP client.
/// Mounted on port :8888.
#[derive(Clone)]
pub struct RelayHandler {
    config: ConfigStore,
    store: ProxyStore,
    events: EventBus,
    http_client: reqwest::Client,
    proxy_clients: Arc<Mutex<HashMap<String, reqwest::Client>>>,
    retry_count: u32,
    request_timeout_secs: u64,
}

impl RelayHandler {
    /// Create a new RelayHandler.
    pub fn new(
        config: ConfigStore,
        store: ProxyStore,
        events: EventBus,
        client: reqwest::Client,
    ) -> Self {
        Self {
            config,
            store,
            events,
            http_client: client,
            proxy_clients: Arc::new(Mutex::new(HashMap::new())),
            retry_count: 3,
            request_timeout_secs: 120,
        }
    }

    /// Get or create an HTTP client for a given proxy URL.
    /// `None` = direct connection (no proxy).
    fn client_for_proxy(&self, proxy_url: Option<&str>) -> reqwest::Client {
        match proxy_url {
            None | Some("") => self.http_client.clone(),
            Some(url) => {
                let mut cache = self.proxy_clients.lock().unwrap();
                if let Some(client) = cache.get(url) {
                    return client.clone();
                }
                let client = match reqwest::Proxy::all(url) {
                    Ok(proxy) => reqwest::Client::builder()
                        .proxy(proxy)
                        .connect_timeout(std::time::Duration::from_secs(30))
                        .pool_idle_timeout(std::time::Duration::from_secs(90))
                        .build()
                        .unwrap_or_else(|_| self.http_client.clone()),
                    Err(_) => self.http_client.clone(),
                };
                cache.insert(url.to_string(), client.clone());
                client
            }
        }
    }

    /// Set retry / timeout from proxy config.
    pub fn with_retry_config(mut self, retry_count: u32, timeout_secs: u64) -> Self {
        self.retry_count = retry_count;
        self.request_timeout_secs = timeout_secs;
        self
    }

    /// Return an axum Router for proxy traffic (mount on :8888).
    pub fn build_router(self) -> axum::Router {
        axum::Router::new()
            .fallback(proxy_handler)
            .with_state(self)
    }
}

/// Main proxy handler: detect CONNECT vs forward vs reverse and dispatch.
async fn proxy_handler(
    State(relay): State<RelayHandler>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response<Body> {
    if method == Method::CONNECT {
        return handle_connect_tunnel(relay, uri).await;
    }

    let is_forward = uri.scheme().is_some();
    if is_forward {
        handle_forward_proxy(relay, method, uri, headers, body).await
    } else {
        handle_reverse_proxy(relay, method, uri, headers, body).await
    }
}

// ── CONNECT tunnel ──

async fn handle_connect_tunnel(_relay: RelayHandler, _uri: Uri) -> Response<Body> {
    // CONNECT tunnel requires raw TCP relay which axum doesn't support directly.
    // In practice, Claude Code uses reverse proxy mode (ANTHROPIC_BASE_URL).
    Response::builder()
        .status(StatusCode::NOT_IMPLEMENTED)
        .body(Body::from("CONNECT tunnel not supported in this configuration"))
        .unwrap()
}

// ── Forward proxy (absolute URI) ──

async fn handle_forward_proxy(
    relay: RelayHandler,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response<Body> {
    let upstream_url = format!(
        "{}://{}{}",
        uri.scheme().map(|s| s.as_str()).unwrap_or("https"),
        uri.authority().map(|a| a.as_str()).unwrap_or(""),
        uri.path()
    );

    proxy_request(relay, method, &upstream_url, headers, body).await
}

// ── Reverse proxy (path-based) ──

async fn handle_reverse_proxy(
    relay: RelayHandler,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response<Body> {
    let path = uri.path().to_string();
    proxy_request(relay, method, &path, headers, body).await
}

// ── Core proxy logic ──

async fn proxy_request(
    relay: RelayHandler,
    _method: Method,
    path_or_url: &str,
    headers: HeaderMap,
    body: Bytes,
) -> Response<Body> {
    let start = Instant::now();

    // ── Parse request body ──
    let mut body_json: serde_json::Value =
        match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(e) => {
                return Response::builder()
                    .status(StatusCode::BAD_REQUEST)
                    .body(Body::from(format!("Invalid JSON: {}", e)))
                    .unwrap();
            }
        };

    let request_model = body_json
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();

    let is_streaming = body_json
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let session_id_str = upstream::extract_session_id(&body_json)
        .unwrap_or_else(|| "unknown".to_string());
    let session_id = SessionId::new(session_id_str.clone());

    let msg_count = body_json
        .get("messages")
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0);

    // ── Resolve route via ConfigStore ──
    let route = match relay.config.resolve_route(&request_model).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("[relay] route resolution failed: {}", e);
            return Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(Body::from(format!("Route resolution failed: {}", e)))
                .unwrap();
        }
    };

    let sid_short = if session_id_str.len() > 8 {
        &session_id_str[session_id_str.len() - 8..]
    } else {
        &session_id_str
    };
    tracing::info!(
        "[relay] {}:{} => [{}:{}]",
        sid_short,
        request_model,
        route.provider,
        route.resolved_model,
    );
    tracing::info!(
        "[proxy] [{}] {}[{}] => [{}:{}]",
        sid_short,
        request_model,
        msg_count,
        route.provider,
        route.resolved_model,
    );

    // ── Resolve billing snapshot ──
    let billing = match relay
        .config
        .resolve_billing(&route.provider, &route.resolved_model)
        .await
    {
        Ok(b) => b,
        Err(_) => {
            // Use zero pricing if no pricing config found
            proxy_common::BillingSnapshot {
                pricing_model_id: "unknown".into(),
                provider: route.provider.clone(),
                resolved_model: route.resolved_model.clone(),
                rates: proxy_common::PriceRates::default(),
                currency: "USD".into(),
            }
        }
    };

    // ── Find provider URL and token ──
    let config_snapshot = relay.config.get().await;
    let provider = config_snapshot
        .proxy
        .providers
        .iter()
        .find(|p| p.name == route.provider);
    let provider_url = provider.map(|p| p.url.clone()).unwrap_or_default();
    let provider_token = provider.and_then(|p| p.token.clone());

    // ── Resolve proxy URL ──
    let proxy_url: Option<String> = provider
        .and_then(|p| p.proxy.as_ref())
        .map(|proxy_str| {
            if proxy_str.is_empty() {
                None  // Explicit "no proxy"
            } else {
                Some(proxy_str.clone())  // Per-provider proxy override
            }
        })
        .unwrap_or_else(|| {
            // Provider has no proxy setting → inherit global http_proxy or None
            config_snapshot.proxy.http_proxy.clone()
        });

    // ── Effort injection ──
    let effort = route.effort.clone().unwrap_or_default();
    if !effort.is_empty() && effort != "auto" {
        upstream::inject_effort(&mut body_json, &effort);
    }

    // ── Build upstream URL ──
    let upstream_url = if path_or_url.starts_with("http") {
        // Forward proxy: use full URL
        path_or_url.to_string()
    } else {
        // Reverse proxy: append path to provider base URL
        let base = provider_url.trim_end_matches('/');
        let path = path_or_url.trim_start_matches('/');
        format!("{}/{}", base, path)
    };

    // ── Build upstream headers ──
    let mut upstream_headers = upstream::build_upstream_headers(&headers, provider_token.as_deref());
    // Effort beta header
    if let Some(effort_val) = route.effort.as_ref() {
        if !effort_val.is_empty() && effort_val != "auto" {
            upstream_headers.insert(
                "anthropic-beta",
                axum::http::HeaderValue::from_str(&format!("effort-2025-11-24")).unwrap(),
            );
        }
    }

    // ── Serialize modified body ──
    let body_bytes = match serde_json::to_vec(&body_json) {
        Ok(b) => Bytes::from(b),
        Err(e) => {
            return Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Body::from(format!("Failed to serialize body: {}", e)))
                .unwrap();
        }
    };

    // ── Dispatch upstream ──
    let http_client = relay.client_for_proxy(proxy_url.as_deref());
    let response = match upstream::dispatch_upstream(
        &http_client,
        &upstream_url,
        upstream_headers,
        body_bytes,
        relay.request_timeout_secs,
        relay.retry_count,
    )
    .await
    {
        Ok(resp) => resp,
        Err(e) => {
            tracing::error!(
                "[proxy] [{}] upstream dispatch failed: {}",
                sid_short,
                e,
            );

            // Write failed task to store
            let task = NewTask {
                id: None,
                session_defaults: NewSessionDefaults {
                    client_type: ClientType::ClaudeCode,
                    client_session_id: Some(session_id_str.clone()),
                    ..Default::default()
                },
                started_at: chrono::Utc::now().timestamp_millis(),
                first_byte_at: None,
                ended_at: Some(chrono::Utc::now().timestamp_millis()),
                status: TaskStatus::Failed,
                method: "POST".into(),
                path: path_or_url.to_string(),
                request_headers: Some(serde_json::to_value(
                    upstream::redact_headers(&headers),
                )
                .unwrap_or_default()),
                request_body: Some(serde_json::to_string(&body_json).unwrap_or_default()),
                response_headers: None,
                response_body: None,
                http_status_code: None,
                is_streaming: false,
                requested_model: Some(request_model.clone()),
                upstream: Some(route.upstream.clone()),
                billing,
                usage: TaskUsage::default(),
                timing: proxy_store::TaskTiming {
                    duration_ms: Some(start.elapsed().as_millis() as i64),
                    ..Default::default()
                },
                error: Some(proxy_store::TaskError {
                    error_type: "upstream_error".into(),
                    error_message: e.clone(),
                }),
                metadata: serde_json::json!({}),
                messages_count: msg_count as u32,
            };

            if let Err(e) = relay.store.write(&session_id, task) {
                tracing::error!("[relay] failed to write failed task: {}", e);
            }

            let err_msg = WsMessage::RequestUpdated(proxy_common::models::ProxiedRequest {
                error: Some(e.clone()),
                model: Some(request_model),
                ..Default::default()
            });
            relay.events.publish(err_msg);

            return Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(Body::from(e))
                .unwrap();
        }
    };

    // ── Parse response ──
    let upstream_response = if is_streaming {
        upstream::handle_streaming_response(response, start).await
    } else {
        upstream::handle_non_streaming_response(response, start).await
    };

    // ── Log upstream errors ──
    let is_http_error = upstream_response.status_code >= 400;
    if is_http_error {
        let err_detail = upstream_response
            .error
            .as_deref()
            .unwrap_or("no body");
        tracing::warn!(
            "[proxy] [{}] {} responded HTTP {}: {}",
            sid_short,
            route.resolved_model,
            upstream_response.status_code,
            err_detail,
        );
    }

    // ── Write to store (store computes cost internally) ──
    let task = NewTask {
        id: None,
        session_defaults: NewSessionDefaults {
            client_type: ClientType::ClaudeCode,
            client_session_id: Some(session_id_str.clone()),
            ..Default::default()
        },
        started_at: chrono::Utc::now().timestamp_millis() - upstream_response.duration_ms as i64,
        first_byte_at: upstream_response.ttft_ms.map(|ttft| {
            chrono::Utc::now().timestamp_millis() - upstream_response.duration_ms as i64
                + ttft as i64
        }),
        ended_at: Some(chrono::Utc::now().timestamp_millis()),
        status: if upstream_response.error.is_some() || is_http_error {
            TaskStatus::Failed
        } else {
            TaskStatus::Completed
        },
        method: "POST".into(),
        path: path_or_url.to_string(),
        request_headers: Some(
            serde_json::to_value(upstream::redact_headers(&headers)).unwrap_or_default(),
        ),
        request_body: Some(serde_json::to_string(&body_json).unwrap_or_default()),
        response_headers: Some(
            serde_json::to_value(
                upstream_response
                    .response_headers
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
                    .collect::<std::collections::HashMap<_, _>>(),
            )
            .unwrap_or_default(),
        ),
        response_body: upstream_response.content_text.as_ref().map(|text| {
            let mut resp = proxy_common::NormalizedResponse::default();
            resp.text.push(text.clone());
            resp
        }),
        http_status_code: Some(upstream_response.status_code),
        is_streaming,
        requested_model: Some(request_model.clone()),
        upstream: Some(route.upstream.clone()),
        billing,
        usage: TaskUsage {
            input_tokens: upstream_response.input_tokens as u64,
            output_tokens: upstream_response.output_tokens as u64,
            cache_creation_tokens: upstream_response.cache_creation_tokens as u64,
            cache_read_tokens: upstream_response.cache_read_tokens as u64,
        },
        timing: proxy_store::TaskTiming {
            duration_ms: Some(upstream_response.duration_ms as i64),
            ttft_ms: upstream_response.ttft_ms.map(|v| v as i64),
            stop_reason: upstream_response.stop_reason.clone(),
            upstream_message_id: upstream_response.message_id.clone(),
        },
        error: upstream_response.error.as_ref().map(|e| proxy_store::TaskError {
            error_type: "upstream_error".into(),
            error_message: e.clone(),
        }),
        metadata: serde_json::json!({}),
        messages_count: msg_count as u32,
    };

    let store_result = relay.store.write(&session_id, task);

    // ── Log completion ──
    if let Ok(ref t) = store_result {
        let cost_usd = t.cost_microusd as f64 / 1_000_000.0;
        if let Some(ref err) = upstream_response.error {
            tracing::error!(
                "[proxy] [{}] {} HTTP {} err={} in={} out={} dur={}ms",
                sid_short,
                route.resolved_model,
                upstream_response.status_code,
                err,
                upstream_response.input_tokens,
                upstream_response.output_tokens,
                upstream_response.duration_ms,
            );
        } else {
            tracing::info!(
                "[proxy] [{}] {} in={} out={} cache_w={} cache_r={} cost=${:.6} dur={}ms",
                sid_short,
                route.resolved_model,
                upstream_response.input_tokens,
                upstream_response.output_tokens,
                upstream_response.cache_creation_tokens,
                upstream_response.cache_read_tokens,
                cost_usd,
                upstream_response.duration_ms,
            );
        }
    }

        // ── Publish event ──
    let proxied = proxy_common::models::ProxiedRequest {
        id: store_result
            .as_ref()
            .map(|t| t.id.as_str().to_string())
            .unwrap_or_default(),
        // Use resolved model so frontend can look up pricing correctly
        model: Some(route.resolved_model.clone()),
        provider: Some(route.provider),
        content_text: upstream_response.content_text.clone(),
        input_tokens: Some(upstream_response.input_tokens),
        output_tokens: Some(upstream_response.output_tokens),
        cache_creation_input_tokens: Some(upstream_response.cache_creation_tokens),
        cache_read_input_tokens: Some(upstream_response.cache_read_tokens),
        status_code: Some(upstream_response.status_code),
        duration_ms: Some(upstream_response.duration_ms),
        time_to_first_token_ms: upstream_response.ttft_ms,
        stop_reason: upstream_response.stop_reason,
        message_id: upstream_response.message_id,
        session_id: Some(session_id.as_str().to_string()),
        is_streaming,
        error: upstream_response.error.clone(),
        messages_count: Some(msg_count as u32),
        ..Default::default()
    };

    relay.events.publish(WsMessage::NewRequest(proxied));

    // ── Build client response ──
    let mut response_builder = Response::builder().status(upstream_response.status_code);
    let resp_headers = upstream_response.response_headers;
    for (k, v) in resp_headers.iter() {
        let key = k.as_str().to_lowercase();
        if key != "transfer-encoding" && key != "content-encoding" {
            response_builder = response_builder.header(k.clone(), v.clone());
        }
    }
    // For streaming, body is in SSE events; for non-streaming, body is content_text
    let body_str = if is_streaming {
        upstream_response
            .sse_events
            .iter()
            .filter_map(|ev| {
                ev.data.as_ref().map(|d| {
                    if let Some(ref et) = ev.event_type {
                        format!("event: {}\ndata: {}\n\n", et, d)
                    } else {
                        format!("data: {}\n\n", d)
                    }
                })
            })
            .collect::<Vec<_>>()
            .join("")
    } else {
        upstream_response
            .content_text
            .unwrap_or_default()
    };

    response_builder
        .body(Body::from(body_str))
        .unwrap_or_else(|_| {
            Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Body::from("Failed to build response"))
                .unwrap()
        })
}
