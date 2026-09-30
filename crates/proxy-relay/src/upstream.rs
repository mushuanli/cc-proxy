//! Upstream dispatch utilities: header filtering, SSE parsing, retry logic.
//!
//! Extracted from proxy-server's proxy.rs to keep the relay handler focused.

use std::collections::HashMap;
use std::time::Instant;

use crate::sse::SseParser;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method};
use bytes::Bytes;
use proxy_common::models::{NormalizedResponse, SseEvent, ToolCallRecord};
use proxy_common::UpstreamAuth;

// ── Header constants ──

const REDACTED_HEADERS: &[&str] = &["x-api-key", "authorization"];
const DROP_HEADERS: &[&str] = &["transfer-encoding", "content-encoding", "content-length"];

// ── Header helpers ──

/// Redact sensitive header values for storage.
pub fn redact_headers(headers: &HeaderMap) -> HashMap<String, String> {
    headers
        .iter()
        .filter(|(k, _)| !DROP_HEADERS.contains(&k.as_str().to_lowercase().as_str()))
        .map(|(k, v)| {
            let key = k.as_str().to_lowercase();
            let value = if REDACTED_HEADERS.contains(&key.as_str()) {
                "[REDACTED]".to_string()
            } else {
                v.to_str().unwrap_or("[binary]").to_string()
            };
            (k.to_string(), value)
        })
        .collect()
}

/// Build upstream request headers: strip hop-by-hop headers, inject provider token.
pub fn build_upstream_headers(headers: &HeaderMap, override_token: Option<&str>) -> HeaderMap {
    let mut fwd = HeaderMap::new();
    for (k, v) in headers.iter() {
        let key = k.as_str().to_lowercase();
        if matches!(
            key.as_str(),
            "host"
                | "connection"
                | "transfer-encoding"
                | "content-length"
                | "accept-encoding"
                | "proxy-connection"
                | "proxy-authorization"
        ) {
            continue;
        }
        if override_token.is_some() && (key == "authorization" || key == "x-api-key") {
            continue;
        }
        fwd.insert(k.clone(), v.clone());
    }
    if let Some(token) = override_token {
        if token.starts_with("sk-") {
            fwd.insert(
                "authorization",
                HeaderValue::from_str(&format!("Bearer {}", token)).unwrap(),
            );
        } else {
            fwd.insert("x-api-key", HeaderValue::from_str(token).unwrap());
        }
    }
    // Tell upstream to send uncompressed data so SSE parsing doesn't need to
    // handle compressed streams.
    fwd.insert("accept-encoding", HeaderValue::from_static("identity"));
    fwd
}

/// Drop headers that describe the upstream body's framing.
///
/// Any translation that changes the body length (or its encoding) invalidates
/// both, so they must not be forwarded to the client.
fn strip_body_framing_headers(headers: &mut HeaderMap) {
    headers.remove(reqwest::header::CONTENT_LENGTH);
    headers.remove(reqwest::header::CONTENT_ENCODING);
}

/// Apply authentication material onto a header set, replacing any client-supplied
/// credentials.
///
/// Mechanism-agnostic: `proxy-relay` knows only [`UpstreamAuth`], never OAuth or
/// plan accounts (see `proxy-planx`).
pub fn apply_auth(headers: &mut HeaderMap, auth: &UpstreamAuth) {
    headers.remove("authorization");
    headers.remove("x-api-key");
    match auth {
        // The `sk-` → `Bearer` / otherwise `x-api-key` rule lives in
        // `UpstreamAuth::header_pairs`, so the relay and the account mechanism
        // cannot drift apart on where a static token belongs.
        UpstreamAuth::Static(_) => {
            for (name, value) in auth.header_pairs() {
                insert_header(headers, &name, &value);
            }
        }
        UpstreamAuth::Headers { set, append } => {
            for (name, value) in set {
                insert_header(headers, name, value);
            }
            for (name, value) in append {
                append_header_value(headers, name, value);
            }
        }
    }
}

/// Insert one `(name, value)` pair, warning instead of panicking on a header the
/// mechanism produced but `http` rejects.
fn insert_header(headers: &mut HeaderMap, name: &str, value: &str) {
    match (
        HeaderName::from_bytes(name.as_bytes()),
        HeaderValue::from_str(value),
    ) {
        (Ok(name), Ok(value)) => {
            headers.insert(name, value);
        }
        _ => tracing::warn!("[proxy] dropping invalid auth header '{}'", name),
    }
}

/// Merge `value` into a comma-separated header, preserving existing entries and
/// skipping duplicates.
///
/// Used for `anthropic-beta`: Claude subscription credentials must advertise
/// `oauth-2025-04-20` without dropping betas the client already sent.
fn append_header_value(headers: &mut HeaderMap, name: &str, value: &str) {
    let (Ok(name), Ok(value)) = (
        HeaderName::from_bytes(name.as_bytes()),
        HeaderValue::from_str(value),
    ) else {
        tracing::warn!("[proxy] dropping invalid append header '{}'", name);
        return;
    };

    let existing = headers
        .get(&name)
        .and_then(|current| current.to_str().ok())
        .unwrap_or("")
        .trim()
        .to_string();

    if existing.is_empty() {
        headers.insert(name, value);
        return;
    }
    if existing
        .split(',')
        .any(|part| part.trim() == value.to_str().unwrap_or(""))
    {
        return; // already advertised
    }
    if let Ok(merged) =
        HeaderValue::from_str(&format!("{existing},{}", value.to_str().unwrap_or("")))
    {
        headers.insert(name, merged);
    }
}

/// Build upstream headers and apply authentication in one step.
///
/// With `None` and with `Static`, output is byte-identical to
/// [`build_upstream_headers`] — the plan branch layers identity headers on top
/// of the plain forwarded set.
pub fn build_upstream_headers_with_auth(
    headers: &HeaderMap,
    auth: Option<&UpstreamAuth>,
) -> HeaderMap {
    match auth {
        Some(UpstreamAuth::Headers { .. }) => {
            // Start from the untouched forward set (keeps the client's other
            // headers), then replace the credentials.
            let mut fwd = build_upstream_headers(headers, None);
            if let Some(auth) = auth {
                apply_auth(&mut fwd, auth);
            }
            fwd
        }
        Some(UpstreamAuth::Static(token)) => build_upstream_headers(headers, Some(token)),
        None => build_upstream_headers(headers, None),
    }
}

/// Re-apply auth to already-built headers (used by the 401 retry path so that
/// effort/beta headers added later are preserved).
pub fn apply_auth_to(mut headers: HeaderMap, auth: &UpstreamAuth) -> HeaderMap {
    apply_auth(&mut headers, auth);
    headers
}

/// API payload family used for session tracking and response inspection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiProtocol {
    Anthropic,
    Codex,
}

impl ApiProtocol {
    pub fn request_type(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::Codex => "codex",
        }
    }

    /// Bridge between the relay's internal enum and the public seam type.
    pub fn to_wire(self) -> proxy_common::protocol::WireProtocol {
        match self {
            Self::Anthropic => proxy_common::protocol::WireProtocol::Anthropic,
            Self::Codex => proxy_common::protocol::WireProtocol::Codex,
        }
    }

    /// The protocol a provider speaks when it is not the client's.
    pub fn other(self) -> Self {
        match self {
            Self::Anthropic => Self::Codex,
            Self::Codex => Self::Anthropic,
        }
    }

    /// Default upstream path for a protocol (used when bridging).
    fn default_path(self) -> &'static str {
        match self {
            Self::Anthropic => "/v1/messages",
            Self::Codex => "/responses",
        }
    }
}

/// A planned cross-protocol bridge: the client speaks one protocol, the
/// upstream another, and an injected adapter can translate between them.
pub struct BridgePlan {
    /// Protocol the upstream will actually receive.
    pub target: ApiProtocol,
    /// Path to use instead of the client's.
    pub path: &'static str,
    pub adapter: std::sync::Arc<dyn proxy_common::protocol::ProtocolAdapter>,
}

/// Decide whether a request must be bridged to reach this provider.
///
/// Returns `None` when the provider already serves the client protocol (the
/// normal case, including providers with an empty allow-list), or when no
/// adapter covers the pair.
pub fn plan_bridge(
    client: ApiProtocol,
    provider: &proxy_common::Provider,
    adapter: Option<&std::sync::Arc<dyn proxy_common::protocol::ProtocolAdapter>>,
) -> Option<BridgePlan> {
    if provider.serves(client.request_type()) {
        return None;
    }
    let target = client.other();
    if !provider.serves(target.request_type()) {
        return None;
    }
    let adapter = adapter?;
    adapter
        .supports(client.to_wire(), target.to_wire())
        .then(|| BridgePlan {
            target,
            path: target.default_path(),
            adapter: adapter.clone(),
        })
}

pub fn detect_protocol(path: &str, body: &serde_json::Value) -> ApiProtocol {
    if path.contains("/responses")
        || (body.get("input").is_some() && body.get("messages").is_none())
    {
        ApiProtocol::Codex
    } else {
        ApiProtocol::Anthropic
    }
}

// ── Session ID extraction ──

/// Extract session_id from Anthropic API request body metadata.
pub fn extract_session_id(body_json: &serde_json::Value) -> Option<String> {
    parse_user_id_metadata(body_json).and_then(|inner| {
        inner
            .get("session_id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from)
    })
}

/// Session metadata extracted from the request (headers + Anthropic metadata).
#[derive(Clone, Debug)]
pub struct SessionMetadata {
    pub cwd: Option<String>,
    pub project_key: Option<String>,
}

/// Extract session metadata (cwd, project_key) from request headers and body.
pub fn extract_session_metadata(
    headers: &HeaderMap,
    body_json: &serde_json::Value,
) -> SessionMetadata {
    // Check custom headers first
    let cwd = headers
        .get("x-cwd")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
        .map(String::from);

    let project_key = headers
        .get("x-project-key")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
        .map(String::from);

    // Fall back to Anthropic metadata.user_id JSON
    if let Some(inner) = parse_user_id_metadata(body_json) {
        if cwd.is_none() {
            if let Some(c) = inner.get("cwd").and_then(|v| v.as_str()) {
                return SessionMetadata {
                    cwd: Some(c.to_string()),
                    project_key: project_key.or_else(|| {
                        inner
                            .get("project_key")
                            .and_then(|v| v.as_str())
                            .map(String::from)
                    }),
                };
            }
        }
        if project_key.is_none() {
            if let Some(pk) = inner.get("project_key").and_then(|v| v.as_str()) {
                return SessionMetadata {
                    cwd,
                    project_key: Some(pk.to_string()),
                };
            }
        }
    }

    SessionMetadata { cwd, project_key }
}

/// Parse the metadata.user_id JSON string from an Anthropic request body.
fn parse_user_id_metadata(body_json: &serde_json::Value) -> Option<serde_json::Value> {
    body_json
        .get("metadata")
        .and_then(|m| m.get("user_id"))
        .and_then(|v| v.as_str())
        .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
}

pub fn extract_request_session_id(
    protocol: ApiProtocol,
    headers: &HeaderMap,
    body: &serde_json::Value,
) -> Option<String> {
    for name in [
        "x-claude-code-session-id",
        "session_id",
        "x-session-id",
        "x-codex-session-id",
    ] {
        if let Some(value) = headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .filter(|v| !v.is_empty())
        {
            return Some(value.to_string());
        }
    }
    if protocol == ApiProtocol::Anthropic {
        return extract_session_id(body);
    }
    body.pointer("/metadata/session_id")
        .or_else(|| body.get("session_id"))
        .or_else(|| body.get("conversation_id"))
        .or_else(|| body.get("prompt_cache_key"))
        .and_then(|v| v.as_str())
        .filter(|v| !v.is_empty())
        .map(String::from)
}

pub fn message_count(protocol: ApiProtocol, body: &serde_json::Value) -> u32 {
    let field = match protocol {
        ApiProtocol::Anthropic => "messages",
        ApiProtocol::Codex => "input",
    };
    body.get(field).and_then(|v| v.as_array()).map_or_else(
        || u32::from(body.get(field).is_some()),
        |items| items.len() as u32,
    )
}

// ── Response from upstream dispatch ──

/// Streaming context: carries session metadata needed to emit observations.
#[derive(Clone)]
pub struct StreamCtx {
    pub call_id: String,
    pub session_id: String,
    pub ingest: Option<std::sync::Arc<dyn proxy_session::SessionIngest>>,
}

impl StreamCtx {
    /// Record a full observation, logging but not propagating failures.
    fn record_obs(&self, obs: proxy_session::Observation) {
        let Some(ingest) = &self.ingest else { return };
        if let Err(e) = ingest.record(obs) {
            tracing::warn!("[proxy] failed to record observation: {}", e);
        }
    }
}

/// Result of dispatching a request upstream.
pub struct UpstreamResponse {
    pub status_code: u16,
    pub response_headers: HeaderMap,
    pub content_text: Option<String>,
    pub raw_body: Bytes,
    pub normalized: NormalizedResponse,
    pub sse_events: Vec<SseEvent>,
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub cache_creation_tokens: u32,
    pub cache_read_tokens: u32,
    pub stop_reason: Option<String>,
    pub message_id: Option<String>,
    pub duration_ms: u64,
    pub ttft_ms: Option<u64>,
    pub error: Option<String>,
    pub capture_truncated: bool,
}

const MAX_CAPTURE_BYTES: usize = 4 * 1024 * 1024;
const MAX_CAPTURE_EVENTS: usize = 4096;
const MAX_CAPTURE_TEXT_BYTES: usize = 1024 * 1024;

fn append_limited(target: &mut Vec<u8>, data: &[u8], limit: usize) -> bool {
    let remaining = limit.saturating_sub(target.len());
    let take = remaining.min(data.len());
    target.extend_from_slice(&data[..take]);
    take < data.len()
}

fn push_text_limited(target: &mut String, text: &str, limit: usize) -> bool {
    let remaining = limit.saturating_sub(target.len());
    if text.len() <= remaining {
        target.push_str(text);
        return false;
    }
    let mut end = remaining;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    target.push_str(&text[..end]);
    true
}

// ── Dispatch ──

/// Execute an upstream request with retry logic.
pub async fn dispatch_upstream(
    client: &reqwest::Client,
    method: Method,
    url: &str,
    headers: HeaderMap,
    body: Bytes,
    timeout_secs: u64,
    retry_count: u32,
) -> Result<reqwest::Response, String> {
    let mut last_err = String::new();

    for attempt in 0..=retry_count {
        if attempt > 0 {
            let delay_ms = 200u64 * 2u64.pow(attempt - 1);
            tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
        }

        let req = match client
            .request(method.clone(), url)
            .headers(headers.clone())
            .body(body.clone())
            .timeout(std::time::Duration::from_secs(timeout_secs.max(1)))
            .build()
        {
            Ok(r) => r,
            Err(e) => {
                last_err = format!("build error: {:?}", e);
                continue;
            }
        };

        match client.execute(req).await {
            Ok(resp) => return Ok(resp),
            Err(e) => {
                last_err = format!("{:?}", e);
                // Only retry on connect/timeout errors
                if !e.is_connect() && !e.is_timeout() {
                    return Err(last_err);
                }
            }
        }
    }

    Err(last_err)
}

/// Streaming response with tee: chunks forwarded to client immediately,
/// metadata collected in background for store recording.
pub struct StreamingResponse {
    pub status_code: u16,
    pub body: axum::body::Body,
    pub metadata: tokio::sync::oneshot::Receiver<UpstreamResponse>,
}

/// Build the protocol parser for a request.
fn make_parser(protocol: ApiProtocol) -> Box<dyn proxy_session::ClientParser> {
    match protocol {
        ApiProtocol::Anthropic => Box::new(proxy_session::AnthropicParser::default()),
        ApiProtocol::Codex => Box::new(proxy_session::CodexParser::default()),
    }
}

/// Apply a protocol parser's incremental update to the relay stream state.
#[allow(clippy::too_many_arguments)]
fn apply_stream_update(
    content_text: &mut String,
    normalized: &mut NormalizedResponse,
    input_tokens: &mut u32,
    output_tokens: &mut u32,
    cache_creation_tokens: &mut u32,
    cache_read_tokens: &mut u32,
    stop_reason: &mut Option<String>,
    message_id: &mut Option<String>,
    model: &mut Option<String>,
    error: &mut Option<String>,
    capture_truncated: &mut bool,
    update: proxy_session::StreamUpdate,
    ctx: &StreamCtx,
) {
    if let Some(text) = update.text {
        *capture_truncated |= push_text_limited(content_text, &text, MAX_CAPTURE_TEXT_BYTES);
        let used: usize = normalized.text.iter().map(String::len).sum();
        if used < MAX_CAPTURE_TEXT_BYTES {
            let mut fragment = String::new();
            *capture_truncated |=
                push_text_limited(&mut fragment, &text, MAX_CAPTURE_TEXT_BYTES - used);
            if !fragment.is_empty() {
                normalized.text.push(fragment);
            }
        } else {
            *capture_truncated = true;
        }
    }
    if let Some(thinking) = update.thinking {
        if !thinking.is_empty() {
            normalized.thinking.push(thinking);
        }
    }
    if let Some(v) = update.input_tokens {
        *input_tokens = v;
    }
    if let Some(v) = update.output_tokens {
        *output_tokens = v;
    }
    if let Some(v) = update.cache_creation_tokens {
        *cache_creation_tokens = v;
    }
    if let Some(v) = update.cache_read_tokens {
        *cache_read_tokens = v;
    }
    if let Some(v) = update.stop_reason {
        *stop_reason = Some(v);
    }
    if let Some(v) = update.message_id {
        *message_id = Some(v);
    }
    if let Some(v) = update.model {
        *model = Some(v);
    }
    if let Some(v) = update.error {
        *error = Some(v);
    }
    for obs in update.observations {
        ctx.record_obs(obs);
    }
}

/// Handle a streaming response by teeing chunks:
/// - Forward each chunk to the client via mpsc → Body::from_stream()
/// - Collect all chunks for SSE parsing and recording
/// - Send parsed metadata via oneshot when complete
pub fn stream_upstream_response(
    response: reqwest::Response,
    start: Instant,
    protocol: ApiProtocol,
    ctx: StreamCtx,
    response_translator: Option<Box<dyn proxy_common::protocol::ResponseTranslator>>,
) -> StreamingResponse {
    use futures::StreamExt;
    use tokio_stream::wrappers::ReceiverStream;
    let mut response_translator = response_translator;
    // Whether any translated frame was actually produced, so a non-SSE upstream
    // answer can be handled by the whole-body fallback instead.
    let mut translated_any = false;

    let status_code = response.status().as_u16();
    let mut response_headers = response.headers().clone();
    if response_translator.is_some() {
        strip_body_framing_headers(&mut response_headers);
    }
    let (chunk_tx, chunk_rx) = tokio::sync::mpsc::channel::<Bytes>(64);
    let (meta_tx, meta_rx) = tokio::sync::oneshot::channel::<UpstreamResponse>();
    let body = axum::body::Body::from_stream(
        ReceiverStream::new(chunk_rx).map(Result::<Bytes, axum::Error>::Ok),
    );

    tokio::spawn(async move {
        let mut sse_events = Vec::new();
        let mut content_text = String::new();
        let mut input_tokens: u32 = 0;
        let mut output_tokens: u32 = 0;
        let mut cache_creation_tokens: u32 = 0;
        let mut cache_read_tokens: u32 = 0;
        let mut stop_reason: Option<String> = None;
        let mut message_id: Option<String> = None;
        let mut _model: Option<String> = None;
        let mut ttft_ms: Option<u64> = None;
        let mut error: Option<String> = None;
        let mut raw_body = Vec::new();
        let mut capture_truncated = false;
        let mut captured_event_bytes = 0usize;
        let mut normalized = NormalizedResponse::default();
        let mut parser = SseParser::new();
        let mut byte_stream = response.bytes_stream();
        // Protocol parser (strategy) drives the SSE loop; relay owns no event types.
        let mut cp = make_parser(protocol);
        let parse_ctx = proxy_session::ParseContext {
            call_id: ctx.call_id.clone(),
            session_id: ctx.session_id.clone(),
            source: "proxy",
        };

        loop {
            match byte_stream.next().await {
                Some(Ok(chunk)) => {
                    capture_truncated |= append_limited(&mut raw_body, &chunk, MAX_CAPTURE_BYTES);
                    if ttft_ms.is_none() {
                        ttft_ms = Some(start.elapsed().as_millis() as u64);
                    }
                    let events = parser.feed(&chunk);
                    // Forward to the client. With a cross-protocol translator the
                    // upstream frames are re-encoded into the client protocol
                    // instead of being relayed verbatim; the parse above still
                    // runs against the *upstream* protocol, so capture, billing
                    // and session observations are unaffected by translation.
                    if let Some(translator) = response_translator.as_mut() {
                        let mut disconnected = false;
                        for ev in &events {
                            let Some(data) = ev.data.as_deref() else {
                                continue;
                            };
                            for frame in translator.push(data.as_bytes()) {
                                translated_any = true;
                                if chunk_tx.send(Bytes::from(frame)).await.is_err() {
                                    disconnected = true;
                                    break;
                                }
                            }
                            if disconnected {
                                break;
                            }
                        }
                        if disconnected {
                            error = Some("client disconnected".to_string());
                            break;
                        }
                    } else if chunk_tx.send(chunk.clone()).await.is_err() {
                        error = Some("client disconnected".to_string());
                        break;
                    }
                    for ev in &events {
                        let event_bytes = ev
                            .event_type
                            .as_ref()
                            .map_or(0, String::len)
                            .saturating_add(ev.data.as_ref().map_or(0, String::len));
                        if sse_events.len() < MAX_CAPTURE_EVENTS
                            && captured_event_bytes.saturating_add(event_bytes) <= MAX_CAPTURE_BYTES
                        {
                            captured_event_bytes += event_bytes;
                            sse_events.push(ev.clone());
                        } else {
                            capture_truncated = true;
                        }
                        if let Some(data_str) = &ev.data {
                            if let Some(parsed) = parser.parse_message_data(data_str) {
                                let update = cp.feed_sse(&parsed, &parse_ctx);
                                apply_stream_update(
                                    &mut content_text,
                                    &mut normalized,
                                    &mut input_tokens,
                                    &mut output_tokens,
                                    &mut cache_creation_tokens,
                                    &mut cache_read_tokens,
                                    &mut stop_reason,
                                    &mut message_id,
                                    &mut _model,
                                    &mut error,
                                    &mut capture_truncated,
                                    update,
                                    &ctx,
                                );
                            }
                        }
                    }
                }
                Some(Err(e)) => {
                    error = Some(format!("Stream error: {}", e));
                    break;
                }
                None => break,
            }
        }
        // Finish the translated stream:
        //  * nothing translated → the upstream ignored `stream: true` and answered
        //    with a single body; render it as the client protocol's events rather
        //    than leaving the client with an empty stream;
        //  * transport error → report the failure, because a synthetic completion
        //    makes a truncated answer indistinguishable from a finished one;
        //  * otherwise → normal end of stream.
        if let Some(translator) = response_translator.as_mut() {
            let frames = match (translated_any, error.as_deref()) {
                (false, _) => translator.stream_from_complete(&raw_body),
                (true, Some(reason)) => translator.abort(reason),
                (true, None) => translator.finish(),
            };
            for frame in frames {
                let _ = chunk_tx.send(Bytes::from(frame)).await;
            }
        }
        drop(chunk_tx);
        // Mark any in-flight tool uses as abandoned on stream end.
        for obs in cp.finish_stream(&parse_ctx) {
            ctx.record_obs(obs);
        }

        let meta = UpstreamResponse {
            status_code,
            response_headers,
            content_text: if content_text.is_empty() {
                let text = normalized.text.join("");
                (!text.is_empty()).then_some(text)
            } else {
                Some(content_text)
            },
            raw_body: Bytes::from(raw_body),
            normalized,
            sse_events,
            input_tokens,
            output_tokens,
            cache_creation_tokens,
            cache_read_tokens,
            stop_reason,
            message_id,
            duration_ms: start.elapsed().as_millis() as u64,
            ttft_ms,
            error,
            capture_truncated: capture_truncated || parser.was_truncated(),
        };
        let _ = meta_tx.send(meta);
    });

    StreamingResponse {
        status_code,
        body,
        metadata: meta_rx,
    }
}

/// Parse non-streaming response.
pub async fn handle_non_streaming_response(
    response: reqwest::Response,
    start: Instant,
    protocol: ApiProtocol,
    mut response_translator: Option<Box<dyn proxy_common::protocol::ResponseTranslator>>,
) -> UpstreamResponse {
    let status_code = response.status().as_u16();
    let mut response_headers = response.headers().clone();

    let body_bytes = match response.bytes().await {
        Ok(b) => b,
        Err(e) => {
            return UpstreamResponse {
                status_code,
                response_headers,
                content_text: None,
                raw_body: Bytes::new(),
                normalized: NormalizedResponse::default(),
                sse_events: vec![],
                input_tokens: 0,
                output_tokens: 0,
                cache_creation_tokens: 0,
                cache_read_tokens: 0,
                stop_reason: None,
                message_id: None,
                duration_ms: start.elapsed().as_millis() as u64,
                ttft_ms: None,
                error: Some(format!("Failed to read response body: {}", e)),
                capture_truncated: false,
            }
        }
    };

    // Translate before parsing so every downstream consumer (usage extraction,
    // normalization, capture, billing) sees the client protocol's shape.
    let body_bytes = match response_translator.as_mut() {
        Some(translator) => match translator.translate_complete(&body_bytes) {
            Some(translated) => {
                // The upstream's `content-length` / `content-encoding` describe the
                // bytes it sent, not the bytes we are about to send. Forwarding
                // them makes hyper reject the response (length mismatch) or the
                // client mis-decode it.
                strip_body_framing_headers(&mut response_headers);
                Bytes::from(translated)
            }
            None => body_bytes,
        },
        None => body_bytes,
    };

    let body_json: serde_json::Value = match serde_json::from_slice(&body_bytes) {
        Ok(v) => v,
        Err(_) => {
            let body_text = String::from_utf8_lossy(&body_bytes).to_string();
            let is_http_error = status_code >= 400;
            return UpstreamResponse {
                status_code,
                response_headers,
                content_text: Some(body_text.clone()),
                raw_body: body_bytes,
                normalized: NormalizedResponse::default(),
                sse_events: vec![],
                input_tokens: 0,
                output_tokens: 0,
                cache_creation_tokens: 0,
                cache_read_tokens: 0,
                stop_reason: None,
                message_id: None,
                duration_ms: start.elapsed().as_millis() as u64,
                ttft_ms: None,
                error: if is_http_error {
                    Some(format!("HTTP {}: {}", status_code, body_text.trim()))
                } else {
                    None
                },
                capture_truncated: false,
            };
        }
    };

    let (input_tokens, output_tokens, codex_cached) = usage_from_json(&body_json);

    let cache_creation_tokens = body_json
        .get("usage")
        .and_then(|u| u.get("cache_creation_input_tokens"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32;

    let cache_read_tokens = body_json
        .get("usage")
        .and_then(|u| u.get("cache_read_input_tokens"))
        .and_then(|v| v.as_u64())
        .unwrap_or(codex_cached as u64) as u32;

    let stop_reason = body_json
        .get("stop_reason")
        .and_then(|v| v.as_str())
        .map(String::from);

    let message_id = body_json
        .get("id")
        .and_then(|v| v.as_str())
        .map(String::from);

    let anthropic_text = body_json
        .get("content")
        .and_then(|c| c.as_array())
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(|b| {
                    b.get("text").and_then(|t| t.as_str()).or_else(|| {
                        b.get("type")
                            .and_then(|t| t.as_str())
                            .filter(|&t| t == "text")
                            .and(b.get("text").and_then(|t| t.as_str()))
                    })
                })
                .collect::<Vec<_>>()
                .join("")
        });
    let normalized = normalize_response_body(protocol, &body_json);
    let content_text = anthropic_text.filter(|s| !s.is_empty()).or_else(|| {
        let text = normalized.text.join("");
        (!text.is_empty()).then_some(text)
    });

    let error = body_json
        .get("error")
        .and_then(|e| e.get("message"))
        .and_then(|v| v.as_str())
        .map(String::from)
        .or_else(|| {
            // HTTP error with valid JSON but no error.message field
            if status_code >= 400 {
                Some(format!("HTTP {} (no error detail)", status_code))
            } else {
                None
            }
        });

    UpstreamResponse {
        status_code,
        response_headers,
        content_text: if content_text.as_deref() == Some("") {
            None
        } else {
            content_text
        },
        raw_body: body_bytes,
        normalized,
        sse_events: vec![],
        input_tokens,
        output_tokens,
        cache_creation_tokens,
        cache_read_tokens,
        stop_reason,
        message_id,
        duration_ms: start.elapsed().as_millis() as u64,
        ttft_ms: None,
        error,
        capture_truncated: false,
    }
}

fn usage_from_json(body: &serde_json::Value) -> (u32, u32, u32) {
    let usage = body.get("usage").unwrap_or(&serde_json::Value::Null);
    let input = usage
        .get("input_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32;
    let output = usage
        .get("output_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32;
    let cached = usage
        .pointer("/input_tokens_details/cached_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32;
    (input, output, cached)
}

fn normalize_response_body(protocol: ApiProtocol, body: &serde_json::Value) -> NormalizedResponse {
    let mut normalized = NormalizedResponse::default();
    if protocol == ApiProtocol::Anthropic {
        for block in body
            .get("content")
            .and_then(|v| v.as_array())
            .into_iter()
            .flatten()
        {
            match block.get("type").and_then(|v| v.as_str()) {
                Some("text") => push_string(block.get("text"), &mut normalized.text),
                Some("thinking") => push_string(block.get("thinking"), &mut normalized.thinking),
                Some("tool_use") => normalized.tool_calls.push(ToolCallRecord {
                    id: string_field(block, "id"),
                    name: string_field(block, "name"),
                    input: block.get("input").cloned().unwrap_or_default(),
                }),
                _ => {}
            }
        }
        return normalized;
    }

    for item in body
        .get("output")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
    {
        match item.get("type").and_then(|v| v.as_str()) {
            Some("message") => {
                for content in item
                    .get("content")
                    .and_then(|v| v.as_array())
                    .into_iter()
                    .flatten()
                {
                    push_string(content.get("text"), &mut normalized.text);
                }
            }
            Some("reasoning") => {
                for summary in item
                    .get("summary")
                    .and_then(|v| v.as_array())
                    .into_iter()
                    .flatten()
                {
                    push_string(summary.get("text"), &mut normalized.thinking);
                }
            }
            Some("function_call") => normalized.tool_calls.push(ToolCallRecord {
                id: string_field(item, "call_id"),
                name: string_field(item, "name"),
                input: item.get("arguments").cloned().unwrap_or_default(),
            }),
            _ => {}
        }
    }
    normalized
}

fn push_string(value: Option<&serde_json::Value>, target: &mut Vec<String>) {
    if let Some(value) = value.and_then(|v| v.as_str()).filter(|v| !v.is_empty()) {
        let used = target.iter().map(String::len).sum::<usize>();
        if used >= MAX_CAPTURE_TEXT_BYTES {
            return;
        }
        let mut fragment = String::new();
        push_text_limited(
            &mut fragment,
            value,
            MAX_CAPTURE_TEXT_BYTES.saturating_sub(used),
        );
        if !fragment.is_empty() {
            target.push(fragment);
        }
    }
}

fn string_field(value: &serde_json::Value, key: &str) -> String {
    value
        .get(key)
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string()
}

// ── Effort injection ──

/// Inject effort into request body and beta header.
pub fn inject_effort(body_json: &mut serde_json::Value, effort: &str) {
    if effort.is_empty() || effort == "auto" {
        return;
    }
    // Set output_config.effort
    body_json["output_config"] = serde_json::json!({"effort": effort});
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;

    #[test]
    fn detects_codex_responses_payload() {
        let body = serde_json::json!({
            "model": "gpt-5.2-codex",
            "input": [{"role": "user", "content": "hello"}],
            "prompt_cache_key": "codex-session-1"
        });
        let protocol = detect_protocol("/v1/responses", &body);
        assert_eq!(protocol, ApiProtocol::Codex);
        assert_eq!(message_count(protocol, &body), 1);
        assert_eq!(
            extract_request_session_id(protocol, &HeaderMap::new(), &body).as_deref(),
            Some("codex-session-1")
        );
    }

    #[test]
    fn normalizes_codex_response_and_usage() {
        let body = serde_json::json!({
            "id": "resp_123",
            "model": "gpt-5.2-codex",
            "usage": {
                "input_tokens": 12,
                "output_tokens": 7,
                "input_tokens_details": {"cached_tokens": 5}
            },
            "output": [{
                "type": "message",
                "content": [{"type": "output_text", "text": "done"}]
            }]
        });
        assert_eq!(usage_from_json(&body), (12, 7, 5));
        assert_eq!(
            normalize_response_body(ApiProtocol::Codex, &body).text,
            ["done"]
        );
    }

    #[test]
    fn extracts_codex_stream_usage_and_delta() {
        use proxy_session::ClientParser;
        let ctx = proxy_session::ParseContext {
            call_id: "call-1".into(),
            session_id: "sess-1".into(),
            source: "proxy",
        };
        let mut parser = proxy_session::CodexParser::default();
        let u1 = parser.feed_sse(
            &serde_json::json!({"type":"response.output_text.delta","delta":"hi"}),
            &ctx,
        );
        let u2 = parser.feed_sse(
            &serde_json::json!({
                "type":"response.completed",
                "response":{"id":"resp_1","model":"gpt-5","usage":{
                    "input_tokens":9,"output_tokens":4,
                    "input_tokens_details":{"cached_tokens":3}
                }}
            }),
            &ctx,
        );
        assert_eq!(u1.text.as_deref(), Some("hi"));
        assert_eq!(u2.input_tokens, Some(9));
        assert_eq!(u2.output_tokens, Some(4));
        assert_eq!(u2.cache_read_tokens, Some(3));
        assert_eq!(u2.message_id.as_deref(), Some("resp_1"));
        assert_eq!(u2.model.as_deref(), Some("gpt-5"));
    }

    #[tokio::test]
    async fn streaming_body_exposes_first_chunk_before_upstream_finishes() {
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::convert::Infallible>>(2);
        tokio::spawn(async move {
            let _ = tx.send(Ok(Bytes::from_static(b"first\n\n"))).await;
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            let _ = tx.send(Ok(Bytes::from_static(b"second\n\n"))).await;
        });
        let response = reqwest::Response::from(
            http::Response::builder()
                .status(200)
                .body(reqwest::Body::wrap_stream(
                    tokio_stream::wrappers::ReceiverStream::new(rx),
                ))
                .unwrap(),
        );
        let started = Instant::now();
        let streaming = stream_upstream_response(
            response,
            started,
            ApiProtocol::Anthropic,
            StreamCtx {
                call_id: "call-1".into(),
                session_id: "sess-1".into(),
                ingest: None,
            },
            None,
        );
        let mut body = streaming.body.into_data_stream();
        let first = tokio::time::timeout(std::time::Duration::from_millis(200), body.next())
            .await
            .expect("first chunk should not wait for complete upstream")
            .unwrap()
            .unwrap();
        assert_eq!(first, Bytes::from_static(b"first\n\n"));
        assert!(started.elapsed() < std::time::Duration::from_millis(250));
    }

    // ── Authentication seam (planx) ──

    fn client_headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("content-type", HeaderValue::from_static("application/json"));
        headers.insert(
            "authorization",
            HeaderValue::from_static("Bearer client-token"),
        );
        headers.insert(
            "anthropic-beta",
            HeaderValue::from_static("effort-2025-11-24"),
        );
        headers
    }

    /// Sorted `(name, value)` pairs, so two HeaderMaps compare exactly
    /// (iteration order is not part of the contract).
    fn sorted_pairs(headers: &HeaderMap) -> Vec<(String, String)> {
        let mut pairs: Vec<(String, String)> = headers
            .iter()
            .map(|(k, v)| {
                (
                    k.as_str().to_string(),
                    v.to_str().unwrap_or("[binary]").to_string(),
                )
            })
            .collect();
        pairs.sort();
        pairs
    }

    #[test]
    fn static_auth_is_byte_identical_to_the_legacy_builder() {
        // The single most important guarantee of this seam: a provider using a
        // static token must produce exactly the same upstream headers as before
        // planx existed. Anything else would be an invasive change.
        let headers = client_headers();
        let legacy = build_upstream_headers(&headers, Some("sk-static"));
        let seamed = build_upstream_headers_with_auth(
            &headers,
            Some(&UpstreamAuth::Static("sk-static".into())),
        );
        assert_eq!(sorted_pairs(&legacy), sorted_pairs(&seamed));

        let legacy_plain = build_upstream_headers(&headers, Some("plain-token"));
        let seamed_plain = build_upstream_headers_with_auth(
            &headers,
            Some(&UpstreamAuth::Static("plain-token".into())),
        );
        assert_eq!(sorted_pairs(&legacy_plain), sorted_pairs(&seamed_plain));
    }

    #[test]
    fn no_auth_is_byte_identical_to_the_legacy_builder() {
        let headers = client_headers();
        let legacy = build_upstream_headers(&headers, None);
        let seamed = build_upstream_headers_with_auth(&headers, None);
        assert_eq!(sorted_pairs(&legacy), sorted_pairs(&seamed));
    }

    #[test]
    fn static_auth_uses_bearer_for_sk_tokens() {
        let headers = client_headers();
        let out = build_upstream_headers_with_auth(
            &headers,
            Some(&UpstreamAuth::Static("sk-static".into())),
        );
        assert_eq!(out.get("authorization").unwrap(), "Bearer sk-static");
        assert!(out.get("x-api-key").is_none());
    }

    #[test]
    fn none_auth_forwards_client_credentials() {
        let headers = client_headers();
        let out = build_upstream_headers_with_auth(&headers, None);
        assert_eq!(out.get("authorization").unwrap(), "Bearer client-token");
    }

    #[test]
    fn non_sk_static_token_uses_api_key_header() {
        let headers = client_headers();
        let out = build_upstream_headers_with_auth(
            &headers,
            Some(&UpstreamAuth::Static("plain-token".into())),
        );
        assert_eq!(out.get("x-api-key").unwrap(), "plain-token");
        assert!(out.get("authorization").is_none());
    }

    #[test]
    fn account_headers_replace_client_credentials_and_add_identity() {
        let headers = client_headers();
        let auth = UpstreamAuth::Headers {
            set: vec![
                ("authorization".into(), "Bearer at-plan".into()),
                ("originator".into(), "codex-tui".into()),
                ("chatgpt-account-id".into(), "ws-1".into()),
            ],
            append: vec![],
        };
        let out = build_upstream_headers_with_auth(&headers, Some(&auth));

        assert_eq!(out.get("authorization").unwrap(), "Bearer at-plan");
        assert!(
            out.get("x-api-key").is_none(),
            "client api key must be dropped"
        );
        assert_eq!(out.get("originator").unwrap(), "codex-tui");
        assert_eq!(out.get("chatgpt-account-id").unwrap(), "ws-1");
        // Unrelated headers survive.
        assert_eq!(out.get("content-type").unwrap(), "application/json");
        assert_eq!(out.get("accept-encoding").unwrap(), "identity");
    }

    #[test]
    fn account_headers_override_a_client_supplied_identity() {
        let mut headers = client_headers();
        headers.insert("originator", HeaderValue::from_static("someone-else"));
        let auth = UpstreamAuth::Headers {
            set: vec![
                ("authorization".into(), "Bearer at".into()),
                ("originator".into(), "codex-tui".into()),
            ],
            append: vec![],
        };
        let out = build_upstream_headers_with_auth(&headers, Some(&auth));
        assert_eq!(out.get("originator").unwrap(), "codex-tui");
    }

    #[test]
    fn append_headers_merge_anthropic_beta_without_losing_client_betas() {
        // Claude subscription credentials must advertise the OAuth beta while
        // keeping whatever Claude Code itself asked for.
        let mut headers = client_headers();
        headers.insert(
            "anthropic-beta",
            HeaderValue::from_static("claude-code-20250219"),
        );
        let auth = UpstreamAuth::Headers {
            set: vec![("authorization".into(), "Bearer sk-ant-oat01-x".into())],
            append: vec![("anthropic-beta".into(), "oauth-2025-04-20".into())],
        };
        let out = build_upstream_headers_with_auth(&headers, Some(&auth));
        let beta = out.get("anthropic-beta").unwrap().to_str().unwrap();
        assert_eq!(beta, "claude-code-20250219,oauth-2025-04-20");
        assert_eq!(out.get("authorization").unwrap(), "Bearer sk-ant-oat01-x");
    }

    #[test]
    fn append_headers_create_the_header_when_absent() {
        // `client_headers()` already carries an `anthropic-beta`, so start from a
        // map that genuinely lacks it.
        let mut headers = HeaderMap::new();
        headers.insert("content-type", HeaderValue::from_static("application/json"));
        let auth = UpstreamAuth::Headers {
            set: vec![("authorization".into(), "Bearer at".into())],
            append: vec![("anthropic-beta".into(), "oauth-2025-04-20".into())],
        };
        let out = build_upstream_headers_with_auth(&headers, Some(&auth));
        assert_eq!(out.get("anthropic-beta").unwrap(), "oauth-2025-04-20");
    }

    #[test]
    fn append_headers_do_not_duplicate_an_existing_value() {
        let mut headers = client_headers();
        headers.insert(
            "anthropic-beta",
            HeaderValue::from_static("oauth-2025-04-20"),
        );
        let auth = UpstreamAuth::Headers {
            set: vec![],
            append: vec![("anthropic-beta".into(), "oauth-2025-04-20".into())],
        };
        let out = build_upstream_headers_with_auth(&headers, Some(&auth));
        assert_eq!(out.get("anthropic-beta").unwrap(), "oauth-2025-04-20");
    }

    #[test]
    fn claude_api_key_goes_to_x_api_key_via_explicit_header() {
        // Family-specific placement is the mechanism's job: the relay just
        // applies the header set it is given.
        let headers = client_headers();
        let auth = UpstreamAuth::Headers {
            set: vec![("x-api-key".into(), "sk-ant-api03-x".into())],
            append: vec![],
        };
        let out = build_upstream_headers_with_auth(&headers, Some(&auth));
        assert_eq!(out.get("x-api-key").unwrap(), "sk-ant-api03-x");
        assert!(
            out.get("authorization").is_none(),
            "client bearer must be dropped"
        );
    }

    #[test]
    fn apply_auth_to_preserves_late_headers_for_401_retry() {
        // The retry path re-applies auth onto already-built headers, so
        // effort/beta headers added after the first build must survive.
        let headers = client_headers();
        let first = build_upstream_headers_with_auth(
            &headers,
            Some(&UpstreamAuth::Static("sk-old".into())),
        );
        assert_eq!(first.get("anthropic-beta").unwrap(), "effort-2025-11-24");

        let retried = apply_auth_to(
            first,
            &UpstreamAuth::Headers {
                set: vec![
                    ("authorization".into(), "Bearer at-new".into()),
                    ("originator".into(), "codex-tui".into()),
                ],
                append: vec![],
            },
        );
        assert_eq!(retried.get("authorization").unwrap(), "Bearer at-new");
        assert_eq!(retried.get("anthropic-beta").unwrap(), "effort-2025-11-24");
        assert_eq!(retried.get("originator").unwrap(), "codex-tui");
    }

    #[test]
    fn invalid_identity_header_is_skipped_not_panicked() {
        let headers = client_headers();
        let auth = UpstreamAuth::Headers {
            // HeaderValue::from_str rejects control characters.
            set: vec![
                ("authorization".into(), "Bearer at".into()),
                ("x-bad".into(), "line\nbreak".into()),
            ],
            append: vec![],
        };
        let out = build_upstream_headers_with_auth(&headers, Some(&auth));
        assert!(out.get("x-bad").is_none());
        assert_eq!(out.get("authorization").unwrap(), "Bearer at");
    }

    // ── Cross-protocol bridge planning ──

    use proxy_common::protocol::{
        ProtocolAdapter, ResponseTranslator, TranslatedRequest, WireProtocol,
    };
    use std::sync::Arc;

    /// Adapter that claims Anthropic ⇄ Codex.
    struct StubAdapter;

    impl ProtocolAdapter for StubAdapter {
        fn supports(&self, from: WireProtocol, to: WireProtocol) -> bool {
            matches!(
                (from, to),
                (WireProtocol::Anthropic, WireProtocol::Codex)
                    | (WireProtocol::Codex, WireProtocol::Anthropic)
            )
        }
        fn translate_request(
            &self,
            _from: WireProtocol,
            _to: WireProtocol,
            body: &[u8],
        ) -> Result<TranslatedRequest, String> {
            Ok(TranslatedRequest {
                body: body.to_vec(),
                stream: false,
                model: None,
            })
        }
        fn response_translator(
            &self,
            _from: WireProtocol,
            _to: WireProtocol,
        ) -> Option<Box<dyn ResponseTranslator>> {
            None
        }
    }

    fn provider_with_protocols(protocols: &[&str]) -> proxy_common::Provider {
        proxy_common::Provider {
            name: "p".into(),
            url: "https://api.example.com".into(),
            codex_url: Some("https://api.example.com/codex".into()),
            token: None,
            proxy: None,
            protocols: protocols.iter().map(|p| p.to_string()).collect(),
            account: None,
        }
    }

    #[test]
    fn no_bridge_when_the_provider_speaks_the_client_protocol() {
        let provider = provider_with_protocols(&["anthropic"]);
        assert!(plan_bridge(
            ApiProtocol::Anthropic,
            &provider,
            Some(&(Arc::new(StubAdapter) as Arc<dyn ProtocolAdapter>))
        )
        .is_none());
    }

    #[test]
    fn no_bridge_for_a_provider_with_an_empty_allow_list() {
        // Empty protocols historically means "serves everything"; bridging must
        // not kick in and change existing behaviour.
        let provider = provider_with_protocols(&[]);
        assert!(plan_bridge(
            ApiProtocol::Anthropic,
            &provider,
            Some(&(Arc::new(StubAdapter) as Arc<dyn ProtocolAdapter>))
        )
        .is_none());
    }

    #[test]
    fn a_codex_only_provider_is_bridged_for_anthropic_clients() {
        let provider = provider_with_protocols(&["codex"]);
        let adapter: Arc<dyn ProtocolAdapter> = Arc::new(StubAdapter);
        let plan = plan_bridge(ApiProtocol::Anthropic, &provider, Some(&adapter))
            .expect("bridge should be planned");
        assert_eq!(plan.target, ApiProtocol::Codex);
        assert_eq!(plan.path, "/responses");
    }

    #[test]
    fn no_bridge_without_an_adapter() {
        let provider = provider_with_protocols(&["codex"]);
        assert!(plan_bridge(ApiProtocol::Anthropic, &provider, None).is_none());
    }

    #[test]
    fn a_provider_serving_neither_protocol_is_not_bridged() {
        let provider = provider_with_protocols(&["gemini"]);
        let adapter: Arc<dyn ProtocolAdapter> = Arc::new(StubAdapter);
        assert!(plan_bridge(ApiProtocol::Anthropic, &provider, Some(&adapter)).is_none());
    }

    #[test]
    fn api_protocol_wire_conversion_round_trips() {
        for protocol in [ApiProtocol::Anthropic, ApiProtocol::Codex] {
            assert_eq!(protocol.to_wire().as_str(), protocol.request_type());
            assert_eq!(
                protocol.other().to_wire().as_str(),
                protocol.other().request_type()
            );
        }
        assert_eq!(ApiProtocol::Anthropic.other(), ApiProtocol::Codex);
        assert_eq!(ApiProtocol::Codex.other(), ApiProtocol::Anthropic);
        assert_eq!(ApiProtocol::Anthropic.default_path(), "/v1/messages");
        assert_eq!(ApiProtocol::Codex.default_path(), "/responses");
    }
}
