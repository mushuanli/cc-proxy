//! RelayHandler — proxy entry point.
//!
//! Handles three proxy modes:
//! 1. CONNECT tunnel (HTTPS forward proxy)
//! 2. Forward proxy (absolute URI in request line)
//! 3. Reverse proxy (path-based, e.g. /v1/messages)
//!
//! Flow: resolve route → dispatch upstream → store.task_write() → events.publish()

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

fn request_prompt(body: &serde_json::Value) -> Option<String> {
    proxy_store::summary::analyzer::extract_latest_user_prompt(body)
}
use proxy_common::{ClientType, SessionId, TaskId, TaskStatus, TaskUsage, WsMessage};
use proxy_common::{ConfigStore, EventBus};
use proxy_common::{ResolvedRoute, AUTO_PROXY_UPSTREAM, FORBID_PROXY_UPSTREAM};
use proxy_store::{summarize_task, summary_current_operation};
use proxy_store::{NewSessionDefaults, NewTask, ProxyStore, StoreResult};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use dashmap::DashMap;

use crate::upstream;
use crate::CaptureControl;

fn effective_proxy(
    provider_proxy: Option<&String>,
    global_proxy: Option<&String>,
) -> Option<String> {
    match provider_proxy {
        Some(proxy) if proxy.is_empty() => None,
        Some(proxy) => Some(proxy.clone()),
        None => global_proxy.filter(|proxy| !proxy.is_empty()).cloned(),
    }
}

/// Proxy relay handler. Owns the config, store, events, and HTTP client.
/// Mounted on port :8888.
#[derive(Clone)]
pub struct RelayHandler {
    config: ConfigStore,
    store: ProxyStore,
    events: EventBus,
    http_client: reqwest::Client,
    proxy_clients: Arc<DashMap<String, reqwest::Client>>,
    retry_count: u32,
    request_timeout_secs: u64,
    capture: CaptureControl,
    session_ingest: Option<Arc<dyn proxy_session::SessionIngest>>,
}

impl RelayHandler {
    /// Create a new RelayHandler.
    pub fn new(
        config: ConfigStore,
        store: ProxyStore,
        events: EventBus,
        client: reqwest::Client,
        capture: CaptureControl,
    ) -> Self {
        Self {
            config,
            store,
            events,
            http_client: client,
            proxy_clients: Arc::new(DashMap::new()),
            retry_count: 3,
            request_timeout_secs: 120,
            capture,
            session_ingest: None,
        }
    }

    /// Attach a session ingest collector (observations feed).
    pub fn with_session_ingest(mut self, ingest: Arc<dyn proxy_session::SessionIngest>) -> Self {
        self.session_ingest = Some(ingest);
        self
    }

    /// Get or create an HTTP client for a given proxy URL.
    /// `None` = direct connection (no proxy).
    fn client_for_proxy(&self, proxy_url: Option<&str>) -> reqwest::Client {
        match proxy_url {
            None | Some("") => self.http_client.clone(),
            Some(url) => self
                .proxy_clients
                .entry(url.to_string())
                .or_insert_with(|| match reqwest::Proxy::all(url) {
                    Ok(proxy) => reqwest::Client::builder()
                        .proxy(proxy)
                        .connect_timeout(std::time::Duration::from_secs(30))
                        .pool_idle_timeout(std::time::Duration::from_secs(90))
                        .build()
                        .unwrap_or_else(|_| self.http_client.clone()),
                    Err(_) => self.http_client.clone(),
                })
                .clone(),
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
        axum::Router::new().fallback(proxy_handler).with_state(self)
    }
}

/// Main proxy handler: detect CONNECT vs forward vs reverse and dispatch.
async fn proxy_handler(
    State(relay): State<RelayHandler>,
    mut request: axum::extract::Request,
) -> Response<Body> {
    let method = request.method().clone();
    let uri = request.uri().clone();
    let is_transparent = method == Method::CONNECT || uri.scheme().is_some();
    if is_transparent
        && relay.config.get().await.proxy.active_proxy_upstream == FORBID_PROXY_UPSTREAM
    {
        return Response::builder()
            .status(StatusCode::FORBIDDEN)
            .body(Body::from("Transparent proxy is forbidden"))
            .unwrap();
    }
    if method == Method::CONNECT {
        return handle_connect_tunnel(&mut request, uri).await;
    }

    let headers = request.headers().clone();
    let body = match axum::body::to_bytes(request.into_body(), 32 * 1024 * 1024).await {
        Ok(body) => body,
        Err(e) => {
            return Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(Body::from(format!("failed to read request body: {}", e)))
                .unwrap()
        }
    };
    let is_forward = uri.scheme().is_some();
    if is_forward {
        handle_forward_proxy(relay, method, uri, headers, body).await
    } else {
        handle_reverse_proxy(relay, method, uri, headers, body).await
    }
}

// ── CONNECT tunnel ──

async fn handle_connect_tunnel(request: &mut axum::extract::Request, uri: Uri) -> Response<Body> {
    let Some(authority) = uri.authority().cloned() else {
        return Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .body(Body::from("CONNECT target must be host:port"))
            .unwrap();
    };
    let host = authority.host();
    if host.is_empty() || authority.as_str().contains('@') {
        return Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .body(Body::from("invalid CONNECT target"))
            .unwrap();
    }
    let target = format!("{}:{}", host, authority.port_u16().unwrap_or(443));
    let upstream = match tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio::net::TcpStream::connect(&target),
    )
    .await
    {
        Ok(Ok(stream)) => stream,
        Ok(Err(e)) => {
            return Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(Body::from(format!("CONNECT failed: {}", e)))
                .unwrap()
        }
        Err(_) => {
            return Response::builder()
                .status(StatusCode::GATEWAY_TIMEOUT)
                .body(Body::from("CONNECT timed out"))
                .unwrap()
        }
    };

    let on_upgrade = hyper::upgrade::on(request);
    tokio::spawn(async move {
        match on_upgrade.await {
            Ok(upgraded) => {
                let mut upgraded = hyper_util::rt::TokioIo::new(upgraded);
                let mut upstream = upstream;
                if let Err(e) = tokio::io::copy_bidirectional(&mut upgraded, &mut upstream).await {
                    tracing::debug!("CONNECT {} closed: {}", target, e);
                }
            }
            Err(e) => tracing::debug!("CONNECT upgrade failed: {}", e),
        }
    });

    Response::builder()
        .status(StatusCode::OK)
        .body(Body::empty())
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
        uri.path_and_query()
            .map(|p| p.as_str())
            .unwrap_or(uri.path())
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
    let path = uri
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or(uri.path())
        .to_string();
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
    let mut body_json: serde_json::Value = match serde_json::from_slice(&body) {
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

    let msg_count = upstream::message_count(protocol, &body_json);
    let prompt = request_prompt(&body_json);

    // ── Resolve route config (needed before session_id for headless fallback) ──
    let config_snapshot = relay.config.get().await;
    let ws_include_bodies = config_snapshot.server.ws_include_bodies;
    let upstream_name = if is_transparent && !config_snapshot.proxy.active_proxy_upstream.is_empty()
    {
        &config_snapshot.proxy.active_proxy_upstream
    } else {
        &config_snapshot.proxy.active_upstream
    };

    // ── Session ID: use header/body value, else fall back to latest recording session ──
    let session_id_str = upstream::extract_request_session_id(protocol, &headers, &body_json);
    let session_id_str = match session_id_str {
        Some(sid) => sid,
        None => relay
            .store
            .session_headless()
            .await
            .ok()
            .flatten()
            .map(|sid| sid.as_str().to_string())
            .unwrap_or_else(|| format!("headless-{}", TaskId::generate())),
    };
    let session_id = SessionId::new(session_id_str.clone())
        .unwrap_or_else(|_| SessionId::from_trusted(format!("headless-{}", TaskId::generate())));

    // Extract session metadata (cwd, project_key) from headers and body
    let session_meta = upstream::extract_session_metadata(&headers, &body_json);

    // ── Resolve route (auto-detect or tier routing) ──
    let use_auto_route = upstream_name == AUTO_PROXY_UPSTREAM && is_transparent;
    let (route, provider_url, provider_token) = if use_auto_route {
        // Auto-detect: find provider whose base URL matches the request URL
        let matched = config_snapshot.proxy.providers.iter().find(|p| {
            let base = p.url.trim_end_matches('/');
            path_or_url.starts_with(base)
        });
        match matched {
            Some(p) => (
                ResolvedRoute {
                    upstream: AUTO_PROXY_UPSTREAM.into(),
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
                    "auto-detect failed: no provider matches request URL: {}",
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
        let route = match relay
            .config
            .resolve_route_for(upstream_name, &request_model)
            .await
        {
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
    let sid_short: String = if sid_s.len() > 8 {
        sid_s[sid_s.len() - 8..].to_string()
    } else {
        sid_s.to_string()
    };
    tracing::info!(
        "[{}] {}[{}] => [{}:{}]",
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
    let proxy_url = effective_proxy(
        proxy_provider.and_then(|p| p.proxy.as_ref()),
        config_snapshot.proxy.http_proxy.as_ref(),
    );

    // ── Effort injection ──
    let effort = route.effort.clone().unwrap_or_default();
    if !is_transparent && !effort.is_empty() && effort != "auto" {
        upstream::inject_effort(&mut body_json, &effort);
    }

    // ── Model name translation ──
    // Replace the original model name with the provider-specific resolved name
    // so the upstream receives the correct model identifier.
    // When resolved_model is empty, pass the original model through unchanged
    // (transparent mode: default has only provider, no model override).
    if !is_transparent && !route.resolved_model.is_empty() && route.resolved_model != request_model {
        body_json["model"] = serde_json::json!(route.resolved_model);
        tracing::debug!(
            "[{}] model translation: {} -> {}",
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
    let override_token = (!is_transparent)
        .then_some(provider_token.as_deref())
        .flatten();
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

    // ── Generate task context before dispatch ──
    let task_id = TaskId::generate();
    let task_started_at =
        chrono::Utc::now().timestamp_millis() - start.elapsed().as_millis() as i64;

    // ── Build Recording task for persistence ──
    let request_headers = upstream::redact_headers(&headers);
    let request_body = stored_request_body(is_transparent, &body, &body_json);
    let recording_metadata = serde_json::json!({
        "protocol": protocol.request_type(),
        "upstream_mode": if is_transparent { "proxy" } else { "relay" },
        "priced": priced,
    });

    let recording_task = NewTask {
        id: Some(task_id.clone()),
        session_defaults: NewSessionDefaults {
            client_type: client_type(protocol),
            client_session_id: Some(session_id_str.clone()),
            cwd: session_meta.cwd.clone(),
            project_key: session_meta.project_key.clone(),
            ..Default::default()
        },
        started_at: task_started_at,
        first_byte_at: None,
        ended_at: None,
        status: TaskStatus::Recording,
        method: method.to_string(),
        path: path_or_url.to_string(),
        request_headers: Some(serde_json::to_value(&request_headers).unwrap_or_default()),
        request_body: Some(request_body.clone()),
        response_headers: None,
        response_body: None,
        http_status_code: None,
        is_streaming,
        requested_model: Some(request_model.clone()),
        upstream: Some(route.upstream.clone()),
        billing: billing.clone(),
        usage: TaskUsage::default(),
        timing: proxy_store::TaskTiming::default(),
        error: None,
        metadata: recording_metadata.clone(),
        messages_count: msg_count,
        prompt_text: prompt.clone(),
        summary_json: None,
    };

    // Apply capture trimming to the recording task before persistence
    let mut recording_task = recording_task;
    finalize_task(&mut recording_task, session_id.as_str(), &relay.capture);
    let current_operation = recording_task
        .summary_json
        .as_deref()
        .and_then(summary_current_operation);

    // ── Build ProxiedRequest for WS NewRequest event ──
    let recording_proxied = proxy_common::models::ProxiedRequest {
        id: task_id.as_str().to_string(),
        timestamp: chrono::DateTime::from_timestamp_millis(task_started_at)
            .unwrap_or_else(chrono::Utc::now),
        method: method.to_string(),
        path: path_or_url.to_string(),
        request_headers: request_headers.clone(),
        request_body: if ws_include_bodies {
            Some(request_body.clone())
        } else {
            None
        },
        model: Some(effective_model.clone()),
        provider: Some(route.provider.clone()),
        is_streaming,
        status: Some(TaskStatus::Recording),
        session_id: Some(session_id.as_str().to_string()),
        request_type: protocol.request_type().into(),
        messages_count: Some(msg_count),
        prompt: prompt.clone(),
        current_operation,
        priced: Some(priced),
        ..Default::default()
    };

    // ── Persist Recording task before dispatching upstream ──
    let _start_result = match start_and_publish(
        &relay.store,
        &relay.events,
        &session_id,
        recording_task,
        recording_proxied,
        relay.session_ingest.clone(),
    )
    .await
    {
        Ok(r) => Some(r),
        Err(e) => {
            tracing::error!(
                "[{}] task_start failed: {} — aborting upstream dispatch",
                sid_short,
                e
            );
            return Response::builder()
                .status(StatusCode::SERVICE_UNAVAILABLE)
                .body(Body::from("Service temporarily unavailable"))
                .unwrap();
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
            tracing::error!("[{}] upstream dispatch failed: {}", sid_short, e);

            let failed_at = chrono::Utc::now().timestamp_millis();
            let duration_ms = start.elapsed().as_millis() as i64;

            let finalization = proxy_store::TaskFinalization {
                status: TaskStatus::Failed,
                first_byte_at: None,
                ended_at: failed_at,
                response_headers: None,
                response_body: None,
                http_status_code: None,
                usage: TaskUsage::default(),
                timing: proxy_store::TaskTiming {
                    duration_ms: Some(duration_ms),
                    ..Default::default()
                },
                error: Some(proxy_store::TaskError {
                    error_type: "upstream_error".into(),
                    error_message: e.clone(),
                }),
                metadata_patch: recording_metadata,
            };

            let proxied = proxy_common::models::ProxiedRequest {
                id: task_id.as_str().to_string(),
                timestamp: chrono::DateTime::from_timestamp_millis(task_started_at)
                    .unwrap_or_else(chrono::Utc::now),
                method: method.to_string(),
                path: path_or_url.to_string(),
                request_headers: request_headers.clone(),
                request_body: if ws_include_bodies {
                    Some(request_body.clone())
                } else {
                    None
                },
                model: Some(effective_model.clone()),
                provider: Some(route.provider.clone()),
                is_streaming: false,
                status: Some(TaskStatus::Failed),
                duration_ms: Some(duration_ms as u64),
                session_id: Some(session_id.as_str().to_string()),
                error: Some(e.clone()),
                request_type: protocol.request_type().into(),
                messages_count: Some(msg_count),
                prompt: prompt.clone(),
                priced: Some(priced),
                ..Default::default()
            };

            finalize_and_publish(
                &relay.store,
                &relay.events,
                &task_id,
                finalization,
                proxied,
                relay.session_ingest.clone(),
            )
            .await;

            return Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(Body::from(e))
                .unwrap();
        }
    };

    // ── Parse response ──
    let upstream_response: upstream::UpstreamResponse;

    if !is_streaming {
        upstream_response =
            upstream::handle_non_streaming_response(response, start, protocol).await;
    } else {
        let stream_ctx = upstream::StreamCtx {
            call_id: task_id.as_str().to_string(),
            session_id: session_id.as_str().to_string(),
            ingest: relay.session_ingest.clone(),
        };
        let stream = upstream::stream_upstream_response(response, start, protocol, stream_ctx);
        let client_resp = Response::builder()
            .status(stream.status_code)
            .body(stream.body)
            .unwrap();
        let stream_meta = stream.metadata;

        let store = relay.store.clone();
        let events = relay.events.clone();
        let ingest = relay.session_ingest.clone();
        let session_id_clone = session_id.clone();
        let log_model = request_model.clone();
        let effective_model_clone = effective_model.clone();
        let method_str = method.to_string();
        let path_str = path_or_url.to_string();
        let _route_upstream = route.upstream.clone();
        let route_provider = route.provider.clone();
        let resolved_model = route.resolved_model.clone();
        let capture = relay.capture.clone();
        let prompt_clone = prompt.clone();
        let ws_include_bodies_val = ws_include_bodies;
        let msg_count_val = msg_count;
        let priced_val = priced;
        let rates_val = billing.rates;
        let is_transparent_val = is_transparent;
        let body_clone = body.clone();
        let body_json_clone = body_json.clone();
        let headers_clone = headers.clone();

        tokio::spawn(async move {
            let meta = match stream_meta.await {
                Ok(m) => m,
                Err(_) => {
                    // Stream interrupted — finalize as Interrupted
                    let now = chrono::Utc::now().timestamp_millis();
                    let finalization = proxy_store::TaskFinalization {
                        status: TaskStatus::Interrupted,
                        first_byte_at: None,
                        ended_at: now,
                        response_headers: None,
                        response_body: None,
                        http_status_code: None,
                        usage: TaskUsage::default(),
                        timing: proxy_store::TaskTiming {
                            duration_ms: Some((now - task_started_at).max(0)),
                            ..Default::default()
                        },
                        error: None,
                        metadata_patch: recording_metadata.clone(),
                    };
                    finalize_and_publish(
                        &store,
                        &events,
                        &task_id,
                        finalization,
                        proxy_common::models::ProxiedRequest {
                            id: task_id.as_str().to_string(),
                            timestamp: chrono::DateTime::from_timestamp_millis(task_started_at)
                                .unwrap_or_else(chrono::Utc::now),
                            method: method_str,
                            path: path_str,
                            model: Some(effective_model_clone),
                            provider: Some(route_provider),
                            is_streaming: true,
                            status: Some(TaskStatus::Interrupted),
                            session_id: Some(session_id_clone.as_str().to_string()),
                            request_type: protocol.request_type().into(),
                            messages_count: Some(msg_count_val),
                            prompt: prompt_clone,
                            priced: Some(priced_val),
                            ..Default::default()
                        },
                        ingest,
                    )
                    .await;
                    return;
                }
            };

            let now = chrono::Utc::now().timestamp_millis();
            let final_status = if meta.error.is_some() || meta.status_code >= 400 {
                TaskStatus::Failed
            } else {
                TaskStatus::Completed
            };

            let finalization = proxy_store::TaskFinalization {
                status: final_status.clone(),
                first_byte_at: meta.ttft_ms.map(|ttft| task_started_at + ttft as i64),
                ended_at: now,
                response_headers: Some(
                    serde_json::to_value(
                        meta.response_headers
                            .iter()
                            .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
                            .collect::<HashMap<String, String>>(),
                    )
                    .unwrap_or_default(),
                ),
                response_body: Some(meta.normalized.clone()),
                http_status_code: Some(meta.status_code),
                usage: TaskUsage {
                    input_tokens: meta.input_tokens as u64,
                    output_tokens: meta.output_tokens as u64,
                    cache_creation_tokens: meta.cache_creation_tokens as u64,
                    cache_read_tokens: meta.cache_read_tokens as u64,
                },
                timing: proxy_store::TaskTiming {
                    duration_ms: Some(meta.duration_ms as i64),
                    ttft_ms: meta.ttft_ms.map(|v| v as i64),
                    stop_reason: meta.stop_reason.clone(),
                    upstream_message_id: meta.message_id.clone(),
                },
                error: meta.error.as_ref().map(|e| proxy_store::TaskError {
                    error_type: "upstream_error".into(),
                    error_message: e.clone(),
                }),
                metadata_patch: serde_json::json!({
                    "protocol": protocol.request_type(),
                    "upstream_mode": if is_transparent_val { "proxy" } else { "relay" },
                    "priced": priced_val,
                    "capture_truncated": meta.capture_truncated,
                    "raw_response_body": String::from_utf8_lossy(&meta.raw_body).to_string(),
                    "sse_events": meta.sse_events.clone(),
                }),
            };

            let current_operation = response_operation_preview(&meta.normalized);
            let cost = if priced_val {
                let micro_usd = (meta.input_tokens as f64 * rates_val.input_microusd as f64
                    + meta.output_tokens as f64 * rates_val.output_microusd as f64
                    + meta.cache_creation_tokens as f64 * rates_val.cache_write_microusd as f64
                    + meta.cache_read_tokens as f64 * rates_val.cache_read_microusd as f64)
                    / 1_000_000.0;
                Some(micro_usd / 1_000_000.0)
            } else {
                None
            };
            let mut proxied = proxy_common::models::ProxiedRequest {
                id: task_id.as_str().to_string(),
                timestamp: chrono::DateTime::from_timestamp_millis(task_started_at)
                    .unwrap_or_else(chrono::Utc::now),
                method: method_str.clone(),
                path: path_str.clone(),
                request_headers: upstream::redact_headers(&headers_clone),
                request_body: Some(stored_request_body(
                    is_transparent_val,
                    &body_clone,
                    &body_json_clone,
                )),
                model: Some(effective_model_clone.clone()),
                provider: Some(route_provider.clone()),
                response_headers: meta
                    .response_headers
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
                    .collect(),
                response_body: serde_json::to_string(&meta.normalized).ok(),
                content_text: meta.content_text.clone(),
                status_code: Some(meta.status_code),
                input_tokens: Some(meta.input_tokens),
                output_tokens: Some(meta.output_tokens),
                cache_creation_input_tokens: Some(meta.cache_creation_tokens),
                cache_read_input_tokens: Some(meta.cache_read_tokens),
                duration_ms: Some(meta.duration_ms),
                time_to_first_token_ms: meta.ttft_ms,
                stop_reason: meta.stop_reason.clone(),
                message_id: meta.message_id.clone(),
                session_id: Some(session_id_clone.as_str().to_string()),
                is_streaming: true,
                status: Some(final_status),
                error: meta.error.clone(),
                request_type: protocol.request_type().into(),
                messages_count: Some(msg_count_val),
                prompt: prompt_clone,
                current_operation,
                priced: Some(priced_val),
                cost,
                sse_events: meta.sse_events.clone(),
                ..Default::default()
            };
            if !ws_include_bodies_val {
                proxied.request_body = None;
                proxied.response_body = None;
                proxied.content_text = None;
                proxied.sse_events.clear();
            }

            finalize_and_publish(&store, &events, &task_id, finalization, proxied, ingest).await;

            capture.record_exchange(
                session_id_clone.as_str(),
                &crate::ExchangeInfo {
                    method: method_str.clone(),
                    path: path_str.clone(),
                    status_code: meta.status_code,
                    request_body: stored_request_body(
                        is_transparent_val,
                        &body_clone,
                        &body_json_clone,
                    ),
                    response_body: meta.content_text.as_deref().unwrap_or("").to_string(),
                    duration_ms: meta.duration_ms,
                },
            );

            if meta.status_code >= 400 {
                let err_detail = meta.error.as_deref().unwrap_or("no body");
                let body_snippet = meta
                    .content_text
                    .as_ref()
                    .map(|t| safe_truncate_bytes(t, 200).to_string())
                    .unwrap_or_default();
                tracing::warn!(
                    "[stream] [{}] {} -> {} HTTP {} err={} body={}",
                    sid_short,
                    log_model,
                    resolved_model,
                    meta.status_code,
                    err_detail,
                    body_snippet,
                );
            } else {
                tracing::info!(
                    "[stream] [{}] {} in={} out={} dur={}ms",
                    sid_short,
                    resolved_model,
                    meta.input_tokens,
                    meta.output_tokens,
                    meta.duration_ms,
                );
            }
        });

        return client_resp;
    }

    // ── Non-streaming: continue with blocking response handling ──
    let is_http_error = upstream_response.status_code >= 400;
    if is_http_error {
        let err_detail = upstream_response.error.as_deref().unwrap_or("no body");
        let body_snippet = upstream_response
            .content_text
            .as_ref()
            .map(|t| safe_truncate_bytes(t, 200).to_string())
            .unwrap_or_default();
        tracing::warn!(
            "[{}] {} -> {} (provider={}) HTTP {} err={} body={}",
            sid_short,
            request_model,
            route.resolved_model,
            route.provider,
            upstream_response.status_code,
            err_detail,
            body_snippet,
        );
    }

    // ── Non-streaming: finalize the Recording task ──
    let response_headers: HashMap<String, String> = upstream_response
        .response_headers
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();

    let final_status = if upstream_response.error.is_some() || is_http_error {
        TaskStatus::Failed
    } else {
        TaskStatus::Completed
    };

    let finalization = proxy_store::TaskFinalization {
        status: final_status.clone(),
        first_byte_at: upstream_response
            .ttft_ms
            .map(|ttft| task_started_at + ttft as i64),
        ended_at: chrono::Utc::now().timestamp_millis(),
        response_headers: Some(serde_json::to_value(&response_headers).unwrap_or_default()),
        response_body: Some(upstream_response.normalized.clone()),
        http_status_code: Some(upstream_response.status_code),
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
        error: upstream_response
            .error
            .as_ref()
            .map(|e| proxy_store::TaskError {
                error_type: "upstream_error".into(),
                error_message: e.clone(),
            }),
        metadata_patch: serde_json::json!({
            "protocol": protocol.request_type(),
            "upstream_mode": if is_transparent { "proxy" } else { "relay" },
            "priced": priced,
            "capture_truncated": upstream_response.capture_truncated,
            "raw_response_body": String::from_utf8_lossy(&upstream_response.raw_body).to_string(),
            "sse_events": upstream_response.sse_events.clone(),
        }),
    };

    let current_operation = response_operation_preview(&upstream_response.normalized);
    let mut proxied = proxy_common::models::ProxiedRequest {
        id: task_id.as_str().to_string(),
        timestamp: chrono::DateTime::from_timestamp_millis(task_started_at)
            .unwrap_or_else(chrono::Utc::now),
        method: method.to_string(),
        path: path_or_url.to_string(),
        request_headers: request_headers.clone(),
        request_body: Some(request_body.clone()),
        model: Some(effective_model.clone()),
        provider: Some(route.provider.clone()),
        response_headers,
        response_body: Some(upstream_response.normalized)
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
        status: Some(final_status),
        error: upstream_response.error.clone(),
        messages_count: Some(msg_count as u32),
        prompt: prompt.clone(),
        current_operation,
        priced: Some(priced),
        request_type: protocol.request_type().into(),
        sse_events: upstream_response.sse_events.clone(),
        ..Default::default()
    };
    if !ws_include_bodies {
        proxied.request_body = None;
        proxied.response_body = None;
        proxied.content_text = None;
        proxied.sse_events.clear();
    }

    finalize_and_publish(
        &relay.store,
        &relay.events,
        &task_id,
        finalization,
        proxied,
        relay.session_ingest.clone(),
    )
    .await;

    // ── Capture recording ──
    relay.capture.record_exchange(
        session_id.as_str(),
        &crate::ExchangeInfo {
            method: method.to_string(),
            path: path_or_url.to_string(),
            status_code: upstream_response.status_code,
            request_body,
            response_body: upstream_response
                .content_text
                .as_deref()
                .unwrap_or("")
                .to_string(),
            duration_ms: upstream_response.duration_ms,
        },
    );

    // ── Log completion ──
    if upstream_response.error.is_some() || is_http_error {
        let err_detail = upstream_response.error.as_deref().unwrap_or("no body");
        tracing::error!(
            "[{}] {} HTTP {} err={} in={} out={} dur={}ms",
            sid_short,
            route.resolved_model,
            upstream_response.status_code,
            err_detail,
            upstream_response.input_tokens,
            upstream_response.output_tokens,
            upstream_response.duration_ms,
        );
    } else {
        let cost_usd = upstream_response.input_tokens as f64 * billing.rates.input_microusd as f64
            / 1_000_000.0
            / 1_000_000.0
            + upstream_response.output_tokens as f64 * billing.rates.output_microusd as f64
                / 1_000_000.0
                / 1_000_000.0;
        tracing::info!(
            "[{}] {} in={} out={} cache_w={} cache_r={} cost=${:.6} dur={}ms",
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

    // ── Build client response ──
    // A body transport failure after a successful status cannot be represented
    // as a valid upstream response.
    if upstream_response.status_code < 400 && upstream_response.error.is_some() {
        let err = upstream_response
            .error
            .as_deref()
            .unwrap_or("upstream body error");
        let err_body = serde_json::json!({
            "error": {
                "type": "proxy_error",
                "message": err
            }
        });
        return Response::builder()
            .status(StatusCode::BAD_GATEWAY)
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::to_string(&err_body).unwrap_or_default(),
            ))
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

/// Generate summary and conditionally strip raw bodies.
///
/// Always generates a TaskSummaryV1 from the request/response bodies.
/// When capture is off, the raw bodies are discarded — only the summary
/// and metadata (tokens, timing, cost, etc.) are persisted.
fn finalize_task(task: &mut NewTask, session_id: &str, capture: &CaptureControl) {
    if let Some(ref body) = task.request_body {
        task.summary_json = summarize_task(
            body,
            task.response_body.as_ref(),
            task.id.as_ref().map(|id| id.as_str()).unwrap_or(""),
            session_id,
            task.status.as_str(),
            &task.billing.resolved_model,
            &task.billing.provider,
            task.upstream.as_deref(),
            task.billing.pricing_model_id != "unknown",
            task.started_at,
            task.http_status_code,
            task.timing.stop_reason.as_deref(),
            task.usage.input_tokens,
            task.usage.output_tokens,
            task.usage.cache_read_tokens,
            task.usage.cache_creation_tokens,
            0, // cost is computed by the store during write
            task.timing.duration_ms,
            task.timing.ttft_ms,
            task.error.as_ref().map(|e| e.error_type.as_str()),
            task.error.as_ref().map(|e| e.error_message.as_str()),
        );
        // Extract prompt_text before request_body is cleared when capture is off
        if task.prompt_text.is_none() {
            task.prompt_text = serde_json::from_str::<serde_json::Value>(body)
                .ok()
                .as_ref()
                .and_then(|v| request_prompt(v));
        }
    }
    if !capture.is_enabled() {
        // Strip request body to only the last user message so the frontend
        // can reuse the same messages-array rendering logic for both modes.
        task.request_body = task
            .request_body
            .as_deref()
            .and_then(strip_body_to_last_user_message);
        task.response_body = None;
    }
}

fn response_operation_preview(response: &proxy_common::NormalizedResponse) -> Option<String> {
    if let Some(tool) = response.tool_calls.last() {
        let input = serde_json::to_string(&tool.input).unwrap_or_default();
        return compact_operation(&format!("{} {}", tool.name, input));
    }
    compact_operation(&response.text.join(" "))
}

fn compact_operation(value: &str) -> Option<String> {
    const MAX_CHARS: usize = 160;
    let normalized = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.is_empty() {
        return None;
    }
    let mut chars = normalized.chars();
    let preview = chars.by_ref().take(MAX_CHARS).collect::<String>();
    Some(if chars.next().is_some() {
        format!("{preview}…")
    } else {
        preview
    })
}

/// Persist a Recording task and publish NewRequest + SessionUpdated.
/// Returns the stored task on success.
async fn start_and_publish(
    store: &ProxyStore,
    events: &EventBus,
    session_id: &SessionId,
    task: NewTask,
    proxied: proxy_common::models::ProxiedRequest,
    ingest: Option<Arc<dyn proxy_session::SessionIngest>>,
) -> StoreResult<proxy_store::TaskStartResult> {
    let result = store.task_start(session_id, task).await?;
    emit_model_call_start(ingest.as_ref(), &result.task);
    events.publish(WsMessage::SessionUpdated(ProxyStore::session_snapshot(
        &result.session,
    )));
    events.publish(WsMessage::NewRequest(proxied));
    Ok(result)
}

/// Emit a ModelCallStart observation carrying the store-assigned sequence_no.
fn emit_model_call_start(
    ingest: Option<&Arc<dyn proxy_session::SessionIngest>>,
    task: &proxy_store::Task,
) {
    let Some(ingest) = ingest else { return };
    let started_at = task.started_at;
    let model = task.resolved_model.clone();
    let session_id = task.session_id.as_str().to_string();
    let call_id = task.id.as_str().to_string();
    let client_request_id = task
        .request_headers
        .as_ref()
        .and_then(|h| h.get("x-request-id").and_then(|v| v.as_str()))
        .map(String::from);
    if let Err(e) = ingest.record(proxy_session::Observation {
        event_id: format!("call-start-{call_id}"),
        session_id: session_id.clone(),
        source: "proxy".into(),
        occurred_at: started_at,
        received_at: chrono::Utc::now().timestamp_millis(),
        source_sequence: Some(task.sequence_no.to_string()),
        source_version: None,
        payload_hash: format!("call-start-{call_id}"),
        kind: proxy_session::ObservationKind::ModelCallStart {
            call_id,
            client_request_id,
            requested_model: if model == "unknown" { None } else { Some(model.clone()) },
            resolved_model: if model == "unknown" { None } else { Some(model) },
            prompt_text: task.prompt_text.clone().map(|t| {
                let mut chars = t.chars();
                let preview: String = chars.by_ref().take(1000).collect();
                if chars.next().is_some() {
                    format!("{preview}…")
                } else {
                    preview
                }
            }),
            started_at,
        },
    }) {
        tracing::warn!("[relay] failed to record model_call_start: {}", e);
    }
}

/// Finalize a Recording task and publish RequestUpdated + SessionUpdated + CostUpdated.
/// Only publishes events if the finalization actually transitioned the task.
#[allow(clippy::too_many_arguments)]
async fn finalize_and_publish(
    store: &ProxyStore,
    events: &EventBus,
    task_id: &TaskId,
    finalization: proxy_store::TaskFinalization,
    proxied: proxy_common::models::ProxiedRequest,
    ingest: Option<Arc<dyn proxy_session::SessionIngest>>,
) {
    match store.task_finalize(task_id, finalization).await {
        Ok(proxy_store::TaskFinalizeResult::Applied { session, task }) => {
            if let Ok(stats) = store.usage_cost_stats().await {
                events.publish(WsMessage::CostUpdated(stats));
            }
            events.publish(WsMessage::SessionUpdated(ProxyStore::session_snapshot(
                &session,
            )));
            events.publish(WsMessage::RequestUpdated(proxied));
            emit_model_call_end(ingest.as_ref(), &task);
        }
        Ok(proxy_store::TaskFinalizeResult::AlreadyFinalized { .. }) => {
            tracing::debug!(
                "[relay] task {} already finalized, skipping events",
                task_id
            );
        }
        Err(e) => {
            tracing::error!("[relay] task_finalize failed for {}: {}", task_id, e);
        }
    }
}

/// Emit a ModelCallEnd observation from a finalized task's frozen fields.
fn emit_model_call_end(
    ingest: Option<&Arc<dyn proxy_session::SessionIngest>>,
    task: &proxy_store::Task,
) {
    let Some(ingest) = ingest else { return };
    let now = chrono::Utc::now().timestamp_millis();
    if let Err(e) = ingest.record(proxy_session::Observation {
        event_id: format!("call-end-{}", task.id),
        session_id: task.session_id.as_str().to_string(),
        source: "proxy".into(),
        occurred_at: now,
        received_at: now,
        source_sequence: None,
        source_version: None,
        payload_hash: format!("call-end-{}", task.id),
        kind: proxy_session::ObservationKind::ModelCallEnd {
            call_id: task.id.as_str().to_string(),
            status: task.status.as_str().to_string(),
            tokens: proxy_session::TokenUsage {
                input_tokens: task.input_tokens,
                output_tokens: task.output_tokens,
                cache_creation_tokens: task.cache_creation_tokens,
                cache_read_tokens: task.cache_read_tokens,
            },
            stop_reason: task.stop_reason.clone(),
            cost_microusd: task.cost_microusd,
            duration_ms: task.duration_ms,
            ended_at: task.ended_at.unwrap_or(now),
            provider_request_id: task.upstream_message_id.clone(),
            error: task
                .error_message
                .clone()
                .filter(|e| !e.is_empty()),
            http_status_code: task.http_status_code,
        },
    }) {
        tracing::warn!("[relay] failed to record model_call_end: {}", e);
    }
}

/// Strip request body to only model, output_config, and the last real user message.
fn strip_body_to_last_user_message(body: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let msg_key = if value.get("messages").and_then(|v| v.as_array()).is_some() {
        "messages"
    } else if value.get("input").and_then(|v| v.as_array()).is_some() {
        "input"
    } else {
        return None;
    };
    let messages = value.get(msg_key)?.as_array()?;

    let last_user_idx = messages.iter().enumerate().rev().find_map(|(i, msg)| {
        let is_user = msg.get("role").and_then(|v| v.as_str()) == Some("user");
        if !is_user {
            return None;
        }
        let is_real = proxy_store::summary::analyzer::is_real_user_prompt(msg);
        if is_real {
            Some(i)
        } else {
            None
        }
    })?;

    let last_user = messages[last_user_idx].clone();
    let mut minimal = serde_json::Map::new();
    minimal.insert(msg_key.to_string(), serde_json::json!([last_user]));
    if let Some(model) = value.get("model") {
        minimal.insert("model".to_string(), model.clone());
    }
    if let Some(oc) = value.get("output_config") {
        minimal.insert("output_config".to_string(), oc.clone());
    }

    Some(serde_json::to_string(&minimal).unwrap_or_default())
}

fn client_type(protocol: upstream::ApiProtocol) -> ClientType {
    match protocol {
        upstream::ApiProtocol::Anthropic => ClientType::ClaudeCode,
        upstream::ApiProtocol::Codex => ClientType::Codex,
    }
}

fn stored_request_body(is_transparent: bool, raw: &Bytes, parsed: &serde_json::Value) -> String {
    if is_transparent {
        String::from_utf8_lossy(raw).to_string()
    } else {
        serde_json::to_string(parsed).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::effective_proxy;

    #[test]
    fn provider_proxy_uses_three_state_inheritance() {
        let global = "http://global:8080".to_string();
        let direct = String::new();
        let override_url = "socks5://provider:1080".to_string();
        assert_eq!(effective_proxy(None, Some(&global)), Some(global.clone()));
        assert_eq!(effective_proxy(Some(&direct), Some(&global)), None);
        assert_eq!(
            effective_proxy(Some(&override_url), Some(&global)),
            Some(override_url)
        );
    }
}
