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

/// Truncate a &str to at most `max_bytes` bytes, preserving UTF-8 boundaries.
fn safe_truncate_bytes(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use proxy_common::{ConfigStore, EventBus};
use proxy_common::{ClientType, SessionId, TaskId, TaskStatus, TaskUsage, WsMessage};
use proxy_common::ResolvedRoute;
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
        uri.path_and_query().map(|p| p.as_str()).unwrap_or(uri.path())
    );

    proxy_request(relay, method, &upstream_url, headers, body, true).await
}

// ── Reverse proxy (path-based) ──

async fn handle_reverse_proxy(
    relay: RelayHandler,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response<Body> {
    let path = uri.path_and_query().map(|p| p.as_str()).unwrap_or(uri.path()).to_string();
    proxy_request(relay, method, &path, headers, body, false).await
}

// ── Core proxy logic ──

async fn proxy_request(
    relay: RelayHandler,
    method: Method,
    path_or_url: &str,
    headers: HeaderMap,
    body: Bytes,
    is_transparent: bool,
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

    let protocol = upstream::detect_protocol(path_or_url, &body_json);
    let request_model = body_json
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();

    let is_streaming = body_json
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let session_id_str = upstream::extract_request_session_id(protocol, &headers, &body_json)
        .unwrap_or_else(|| format!("{}-{}", protocol.request_type(), TaskId::generate()));
    let session_id = SessionId::new(session_id_str.clone())
        .unwrap_or_else(|_| SessionId::from_trusted(
            format!("{}-{}", protocol.request_type(), TaskId::generate())
        ));

    let msg_count = upstream::message_count(protocol, &body_json);

    // ── Resolve route via the upstream assigned to this ingress mode ──
    let config_snapshot = relay.config.get().await;
    let upstream_name = if is_transparent && !config_snapshot.proxy.active_proxy_upstream.is_empty() {
        &config_snapshot.proxy.active_proxy_upstream
    } else {
        &config_snapshot.proxy.active_upstream
    };

    // ── Resolve route (auto-detect or tier routing) ──
    let (route, provider_url, provider_token) = if upstream_name == "__auto__" && is_transparent {
        // Auto-detect: find provider whose base URL matches the request URL
        let matched = config_snapshot.proxy.providers.iter().find(|p| {
            let base = p.url.trim_end_matches('/');
            path_or_url.starts_with(base)
        });
        match matched {
            Some(p) => (
                ResolvedRoute {
                    upstream: "__auto__".into(),
                    provider: p.name.clone(),
                    configured_model: request_model.clone(),
                    resolved_model: request_model.clone(),
                    effort: None,
                },
                p.url.clone(),
                None, // auto mode: do not override API key
            ),
            None => {
                tracing::error!(
                    "[proxy] auto-detect failed: no provider matches request URL: {}",
                    path_or_url
                );
                return Response::builder()
                    .status(StatusCode::BAD_GATEWAY)
                    .body(Body::from(format!(
                        "Auto-detect failed: no configured provider matches request URL\n\nURL: {}",
                        path_or_url
                    )))
                    .unwrap();
            }
        }
    } else {
        // Normal tier-based route resolution
        let route = match relay.config.resolve_route_for(upstream_name, &request_model).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("[relay] route resolution failed: {}", e);
                return Response::builder()
                    .status(StatusCode::BAD_GATEWAY)
                    .body(Body::from(format!("Route resolution failed: {}", e)))
                    .unwrap();
            }
        };
        let provider = config_snapshot
            .proxy
            .providers
            .iter()
            .find(|p| p.name == route.provider);
        let provider_url = provider.map(|p| p.url.clone()).unwrap_or_default();
        let provider_token = provider.and_then(|p| p.token.clone());
        (route, provider_url, provider_token)
    };

    // session_id is validated (ASCII-only), safe for byte slicing
    let sid_s = session_id.as_str();
    let sid_short = if sid_s.len() > 8 {
        &sid_s[sid_s.len() - 8..]
    } else {
        sid_s
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

    let effective_model = if is_transparent {
        request_model.clone()
    } else {
        route.resolved_model.clone()
    };

    // ── Resolve billing snapshot ──
    let mut billing = match relay
        .config
        .resolve_billing(&route.provider, &effective_model)
        .await
    {
        Ok(b) => b,
        Err(_) => {
            // Use zero pricing if no pricing config found
            proxy_common::BillingSnapshot {
                pricing_model_id: "unknown".into(),
                provider: route.provider.clone(),
                resolved_model: effective_model.clone(),
                rates: proxy_common::PriceRates::default(),
                currency: "USD".into(),
            }
        }
    };
    if is_transparent {
        // Pricing lookup must never rewrite the model observed on the wire.
        billing.resolved_model = request_model.clone();
    }
    let priced = billing.pricing_model_id != "unknown";

    // ── Resolve provider proxy config ──
    let proxy_provider = config_snapshot
        .proxy
        .providers
        .iter()
        .find(|p| p.name == route.provider);
    let proxy_url: Option<String> = proxy_provider
        .and_then(|p| p.proxy.as_ref())
        .and_then(|proxy_str| {
            if proxy_str.is_empty() {
                None  // Explicit "no proxy"
            } else {
                Some(proxy_str.clone())  // Per-provider proxy override
            }
        });

    // ── Effort injection ──
    let effort = route.effort.clone().unwrap_or_default();
    if !is_transparent && !effort.is_empty() && effort != "auto" {
        upstream::inject_effort(&mut body_json, &effort);
    }

    // ── Model name translation ──
    // Replace the original model name with the provider-specific resolved name
    // so the upstream receives the correct model identifier.
    if !is_transparent && route.resolved_model != request_model {
        body_json["model"] = serde_json::json!(route.resolved_model);
        tracing::debug!(
            "[relay] [{}] model translation: {} -> {}",
            sid_short,
            request_model,
            route.resolved_model,
        );
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
    let override_token = (!is_transparent).then_some(provider_token.as_deref()).flatten();
    let mut upstream_headers = upstream::build_upstream_headers(&headers, override_token);
    // Effort beta header
    if !is_transparent {
      if let Some(effort_val) = route.effort.as_ref() {
        if !effort_val.is_empty() && effort_val != "auto" {
            upstream_headers.insert(
                "anthropic-beta",
                axum::http::HeaderValue::from_static("effort-2025-11-24"),
            );
        }
      }
    }

    // ── Serialize modified body ──
    let body_bytes = if is_transparent {
        body.clone()
    } else {
        match serde_json::to_vec(&body_json) {
            Ok(bytes) => Bytes::from(bytes),
            Err(e) => {
                return Response::builder()
                    .status(StatusCode::INTERNAL_SERVER_ERROR)
                    .body(Body::from(format!("Failed to serialize body: {}", e)))
                    .unwrap();
            }
        }
    };

    // ── Dispatch upstream ──
    let http_client = relay.client_for_proxy(proxy_url.as_deref());
    let response = match upstream::dispatch_upstream(
        &http_client,
        method.clone(),
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

            // Build the event from the relay result itself. Persistence is a
            // separate concern; the UI must still receive a complete task if
            // the store is unavailable.
            let task_id = TaskId::generate();
            let failed_at = chrono::Utc::now();
            let duration_ms = start.elapsed().as_millis() as u64;
            let request_headers = upstream::redact_headers(&headers);
            let request_body = stored_request_body(is_transparent, &body, &body_json);

            // Write failed task to store
            let task = NewTask {
                id: Some(task_id.clone()),
                session_defaults: NewSessionDefaults {
                    client_type: client_type(protocol),
                    client_session_id: Some(session_id_str.clone()),
                    ..Default::default()
                },
                started_at: failed_at.timestamp_millis(),
                first_byte_at: None,
                ended_at: Some(failed_at.timestamp_millis()),
                status: TaskStatus::Failed,
                method: method.to_string(),
                path: path_or_url.to_string(),
                request_headers: Some(serde_json::to_value(&request_headers).unwrap_or_default()),
                request_body: Some(request_body.clone()),
                response_headers: None,
                response_body: None,
                http_status_code: None,
                is_streaming: false,
                requested_model: Some(request_model.clone()),
                upstream: Some(route.upstream.clone()),
                billing,
                usage: TaskUsage::default(),
                timing: proxy_store::TaskTiming {
                    duration_ms: Some(duration_ms as i64),
                    ..Default::default()
                },
                error: Some(proxy_store::TaskError {
                    error_type: "upstream_error".into(),
                    error_message: e.clone(),
                }),
                metadata: serde_json::json!({
                    "protocol": protocol.request_type(),
                    "upstream_mode": if is_transparent { "proxy" } else { "relay" },
                    "priced": priced,
                }),
                messages_count: msg_count,
            };

            if let Err(store_err) = relay.store.write(&session_id, task) {
                tracing::error!("[relay] failed to write failed task: {}", store_err);
            }

            if let Ok(stats) = relay.store.get_cost_stats() {
                relay.events.publish(WsMessage::CostUpdated(stats));
            }

            let err_msg = WsMessage::RequestUpdated(proxy_common::models::ProxiedRequest {
                id: task_id.as_str().to_string(),
                timestamp: failed_at,
                method: method.to_string(),
                path: path_or_url.to_string(),
                request_headers,
                request_body: Some(request_body),
                error: Some(e.clone()),
                model: Some(effective_model.clone()),
                provider: Some(route.provider.clone()),
                duration_ms: Some(duration_ms),
                session_id: Some(session_id.as_str().to_string()),
                request_type: protocol.request_type().into(),
                messages_count: Some(msg_count),
                priced: Some(priced),
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
        upstream::handle_streaming_response(response, start, protocol).await
    } else {
        upstream::handle_non_streaming_response(response, start, protocol).await
    };

    // ── Log upstream errors ──
    let is_http_error = upstream_response.status_code >= 400;
    if is_http_error {
        let err_detail = upstream_response
            .error
            .as_deref()
            .unwrap_or("no body");
        let body_snippet = upstream_response
            .content_text
            .as_ref()
            .map(|t| safe_truncate_bytes(t, 200).to_string())
            .unwrap_or_default();
        tracing::warn!(
            "[proxy] [{}] {} -> {} (provider={}) HTTP {} err={} body={}",
            sid_short,
            request_model,
            route.resolved_model,
            route.provider,
            upstream_response.status_code,
            err_detail,
            body_snippet,
        );
    }

    // Build one complete task snapshot for both the event and persistence.
    // The event is not reconstructed by querying the store.
    let task_id = TaskId::generate();
    let task_started_at =
        chrono::Utc::now().timestamp_millis() - upstream_response.duration_ms as i64;
    let request_headers = upstream::redact_headers(&headers);
    let request_body = stored_request_body(is_transparent, &body, &body_json);
    let response_headers: HashMap<String, String> = upstream_response
        .response_headers
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();
    let normalized_response = Some(upstream_response.normalized.clone());
    let raw_response_body = String::from_utf8_lossy(&upstream_response.raw_body).to_string();
    let inspect_metadata = serde_json::json!({
        "protocol": protocol.request_type(),
        "upstream_mode": if is_transparent { "proxy" } else { "relay" },
        "priced": priced,
        "raw_response_body": raw_response_body,
        "sse_events": upstream_response.sse_events.clone(),
    });

    // ── Write to store (store computes cost internally) ──
    let task = NewTask {
        id: Some(task_id.clone()),
        session_defaults: NewSessionDefaults {
            client_type: client_type(protocol),
            client_session_id: Some(session_id_str.clone()),
            ..Default::default()
        },
        started_at: task_started_at,
        first_byte_at: upstream_response.ttft_ms.map(|ttft| {
            task_started_at + ttft as i64
        }),
        ended_at: Some(chrono::Utc::now().timestamp_millis()),
        status: if upstream_response.error.is_some() || is_http_error {
            TaskStatus::Failed
        } else {
            TaskStatus::Completed
        },
        method: method.to_string(),
        path: path_or_url.to_string(),
        request_headers: Some(serde_json::to_value(&request_headers).unwrap_or_default()),
        request_body: Some(request_body.clone()),
        response_headers: Some(serde_json::to_value(&response_headers).unwrap_or_default()),
        response_body: normalized_response.clone(),
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
        metadata: inspect_metadata,
        messages_count: msg_count,
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
        id: task_id.as_str().to_string(),
        timestamp: chrono::DateTime::from_timestamp_millis(task_started_at)
            .unwrap_or_else(chrono::Utc::now),
        method: method.to_string(),
        path: path_or_url.to_string(),
        request_headers,
        request_body: Some(request_body),
        // Use resolved model so frontend can look up pricing correctly
        model: Some(effective_model.clone()),
        provider: Some(route.provider.clone()),
        response_headers,
        response_body: normalized_response
            .as_ref()
            .and_then(|body| serde_json::to_string(body).ok()),
        content_text: upstream_response.content_text.clone(),
        input_tokens: Some(upstream_response.input_tokens),
        output_tokens: Some(upstream_response.output_tokens),
        cache_creation_input_tokens: Some(upstream_response.cache_creation_tokens),
        cache_read_input_tokens: Some(upstream_response.cache_read_tokens),
        status_code: Some(upstream_response.status_code),
        duration_ms: Some(upstream_response.duration_ms),
        time_to_first_token_ms: upstream_response.ttft_ms,
        stop_reason: upstream_response.stop_reason.clone(),
        message_id: upstream_response.message_id.clone(),
        session_id: Some(session_id.as_str().to_string()),
        is_streaming,
        error: upstream_response.error.clone(),
        messages_count: Some(msg_count as u32),
        cost: store_result
            .as_ref()
            .ok()
            .map(|task| task.cost_microusd as f64 / 1_000_000.0),
        priced: Some(priced),
        request_type: protocol.request_type().into(),
        sse_events: upstream_response.sse_events.clone(),
        ..Default::default()
    };

    relay.events.publish(WsMessage::NewRequest(proxied));

    // Push real-time cost stats so frontend can update inspector without separate API call
    if let Ok(stats) = relay.store.get_cost_stats() {
        relay.events.publish(WsMessage::CostUpdated(stats));
    }

    // ── Build client response ──
    // A body transport failure after a successful status cannot be represented
    // as a valid upstream response.
    if upstream_response.status_code < 400 && upstream_response.error.is_some() {
        let err = upstream_response.error.as_deref().unwrap_or("upstream body error");
        let err_body = serde_json::json!({
            "error": {
                "type": "proxy_error",
                "message": err
            }
        });
        return Response::builder()
            .status(StatusCode::BAD_GATEWAY)
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&err_body).unwrap_or_default()))
            .unwrap();
    }

    let mut response_builder = Response::builder().status(upstream_response.status_code);
    let resp_headers = upstream_response.response_headers;
    for (k, v) in resp_headers.iter() {
        let key = k.as_str().to_lowercase();
        if key != "transfer-encoding" && key != "content-encoding" {
            response_builder = response_builder.header(k.clone(), v.clone());
        }
    }
    response_builder
        .body(Body::from(upstream_response.raw_body))
        .unwrap_or_else(|_| {
            Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Body::from("Failed to build response"))
                .unwrap()
        })
}

fn client_type(protocol: upstream::ApiProtocol) -> ClientType {
    match protocol {
        upstream::ApiProtocol::Anthropic => ClientType::ClaudeCode,
        upstream::ApiProtocol::Codex => ClientType::Codex,
    }
}

fn stored_request_body(
    is_transparent: bool,
    raw: &Bytes,
    parsed: &serde_json::Value,
) -> String {
    if is_transparent {
        String::from_utf8_lossy(raw).to_string()
    } else {
        serde_json::to_string(parsed).unwrap_or_default()
    }
}
