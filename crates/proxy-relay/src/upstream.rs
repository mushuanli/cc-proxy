//! Upstream dispatch utilities: header filtering, SSE parsing, retry logic.
//!
//! Extracted from proxy-server's proxy.rs to keep the relay handler focused.

use std::collections::HashMap;
use std::time::Instant;

use crate::sse::SseParser;
use axum::http::{HeaderMap, HeaderValue, Method};
use bytes::Bytes;
use proxy_common::models::{NormalizedResponse, SseEvent, ToolCallRecord};

// ── Header constants ──

const REDACTED_HEADERS: &[&str] = &["x-api-key", "authorization"];
const DROP_HEADERS: &[&str] = &["transfer-encoding", "content-encoding", "content-length"];

// ── Header helpers ──

/// Extract hostname from a provider URL for logging.
pub fn extract_host(url: &str) -> Option<&str> {
    url.strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .and_then(|rest| rest.split('/').next())
        .or_else(|| url.split('/').next())
}

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
    body_json
        .get("metadata")
        .and_then(|m| m.get("user_id"))
        .and_then(|v| v.as_str())
        .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
        .and_then(|inner| {
            inner
                .get("session_id")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(String::from)
        })
}

pub fn extract_request_session_id(
    protocol: ApiProtocol,
    headers: &HeaderMap,
    body: &serde_json::Value,
) -> Option<String> {
    for name in ["session_id", "x-session-id", "x-codex-session-id"] {
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
    pub model: Option<String>,
    pub duration_ms: u64,
    pub ttft_ms: Option<u64>,
    pub error: Option<String>,
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

/// Parse streaming SSE response, collecting events and merging content text.
pub async fn handle_streaming_response(
    response: reqwest::Response,
    start: Instant,
    protocol: ApiProtocol,
) -> UpstreamResponse {
    use futures::StreamExt;

    let status_code = response.status().as_u16();
    let response_headers = response.headers().clone();

    // Non-2xx responses are not SSE — read the body as text/JSON for error detail
    if status_code >= 300 {
        let body_bytes = response.bytes().await.unwrap_or_default();
        let body_text = String::from_utf8_lossy(&body_bytes).to_string();
        // Try to extract error message from JSON body
        let error_msg = serde_json::from_str::<serde_json::Value>(&body_text)
            .ok()
            .and_then(|v| {
                v.get("error")
                    .and_then(|e| e.get("message").or_else(|| e.get("type")))
                    .and_then(|v| v.as_str())
                    .map(String::from)
            })
            .unwrap_or_else(|| body_text.trim().to_string());

        return UpstreamResponse {
            status_code,
            response_headers,
            content_text: if body_text.is_empty() {
                None
            } else {
                Some(body_text)
            },
            raw_body: body_bytes,
            normalized: NormalizedResponse::default(),
            sse_events: vec![],
            input_tokens: 0,
            output_tokens: 0,
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            stop_reason: None,
            message_id: None,
            model: None,
            duration_ms: start.elapsed().as_millis() as u64,
            ttft_ms: None,
            error: if error_msg.is_empty() {
                Some(format!("HTTP {} (no body)", status_code))
            } else {
                Some(error_msg)
            },
        };
    }

    let mut sse_events = Vec::new();
    let mut content_text = String::new();
    let mut input_tokens: u32 = 0;
    let mut output_tokens: u32 = 0;
    let mut cache_creation_tokens: u32 = 0;
    let mut cache_read_tokens: u32 = 0;
    let mut stop_reason: Option<String> = None;
    let mut message_id: Option<String> = None;
    let mut model: Option<String> = None;
    let mut ttft_ms: Option<u64> = None;
    let mut error: Option<String> = None;
    let mut raw_body = Vec::new();
    let mut normalized = NormalizedResponse::default();

    let mut parser = SseParser::new();
    let mut byte_stream = response.bytes_stream();

    loop {
        match byte_stream.next().await {
            Some(Ok(chunk)) => {
                raw_body.extend_from_slice(&chunk);
                if ttft_ms.is_none() {
                    ttft_ms = Some(start.elapsed().as_millis() as u64);
                }

                let events = parser.feed(&chunk);
                for ev in &events {
                    sse_events.push(ev.clone());

                    if let Some(data_str) = &ev.data {
                        if let Some(parsed) = parser.parse_message_data(data_str) {
                            if protocol == ApiProtocol::Codex {
                                parse_codex_stream_event(
                                    &parsed,
                                    &mut normalized,
                                    &mut input_tokens,
                                    &mut output_tokens,
                                    &mut cache_read_tokens,
                                    &mut message_id,
                                    &mut model,
                                );
                            }
                            match parser.event_kind(&parsed) {
                                Some("message_start") => {
                                    model = parser.model_from_start(&parsed).map(String::from);
                                    message_id = parser.message_id(&parsed).map(String::from);
                                    input_tokens = parser
                                        .input_tokens_from_start(&parsed)
                                        .unwrap_or(input_tokens);
                                    cache_creation_tokens = parser
                                        .cache_creation_tokens_from_start(&parsed)
                                        .unwrap_or(cache_creation_tokens);
                                    cache_read_tokens = parser
                                        .cache_read_tokens_from_start(&parsed)
                                        .unwrap_or(cache_read_tokens);
                                }
                                Some("message_delta") => {
                                    // Anthropic puts usage next to delta, not inside it.
                                    // output_tokens is cumulative, so keep the latest value.
                                    if let Some(value) = parser.output_tokens_from_delta(&parsed) {
                                        output_tokens = value;
                                    }
                                    if let Some(reason) = parser.stop_reason(&parsed) {
                                        stop_reason = Some(reason.to_string());
                                    }
                                }
                                Some("content_block_delta") => {
                                    if let Some(text) = parser.delta_text(&parsed) {
                                        content_text.push_str(text);
                                        normalized.text.push(text.to_string());
                                    }
                                }
                                Some("error") => {
                                    if let Some(err) = parsed.get("error") {
                                        error = err
                                            .get("message")
                                            .and_then(|v| v.as_str())
                                            .map(String::from);
                                    }
                                }
                                _ => {}
                            }
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

    UpstreamResponse {
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
        model,
        duration_ms: start.elapsed().as_millis() as u64,
        ttft_ms,
        error,
    }
}

/// Streaming response with tee: chunks forwarded to client immediately,
/// metadata collected in background for store recording.
pub struct StreamingResponse {
    pub status_code: u16,
    pub response_headers: HeaderMap,
    pub body: axum::body::Body,
    pub metadata: tokio::sync::oneshot::Receiver<UpstreamResponse>,
}

/// Handle a streaming response by teeing chunks:
/// - Forward each chunk to the client via mpsc → Body::from_stream()
/// - Collect all chunks for SSE parsing and recording
/// - Send parsed metadata via oneshot when complete
pub fn stream_upstream_response(
    response: reqwest::Response,
    start: Instant,
    protocol: ApiProtocol,
) -> StreamingResponse {
    use futures::StreamExt;
    use tokio_stream::wrappers::ReceiverStream;

    let status_code = response.status().as_u16();
    let response_headers = response.headers().clone();
    let resp_headers_for_return = response_headers.clone();
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
        let mut model: Option<String> = None;
        let mut ttft_ms: Option<u64> = None;
        let mut error: Option<String> = None;
        let mut raw_body = Vec::new();
        let mut normalized = NormalizedResponse::default();
        let mut parser = SseParser::new();
        let mut byte_stream = response.bytes_stream();

        loop {
            match byte_stream.next().await {
                Some(Ok(chunk)) => {
                    raw_body.extend_from_slice(&chunk);
                    if ttft_ms.is_none() {
                        ttft_ms = Some(start.elapsed().as_millis() as u64);
                    }
                    // Forward to client; stop if client disconnected
                    if chunk_tx.send(chunk.clone()).await.is_err() {
                        error = Some("client disconnected".to_string());
                        break;
                    }
                    let events = parser.feed(&chunk);
                    for ev in &events {
                        sse_events.push(ev.clone());
                        if let Some(data_str) = &ev.data {
                            if let Some(parsed) = parser.parse_message_data(data_str) {
                                if protocol == ApiProtocol::Codex {
                                    parse_codex_stream_event(
                                        &parsed, &mut normalized, &mut input_tokens,
                                        &mut output_tokens, &mut cache_read_tokens,
                                        &mut message_id, &mut model,
                                    );
                                }
                                match parser.event_kind(&parsed) {
                                    Some("message_start") => {
                                        model = parser.model_from_start(&parsed).map(String::from);
                                        message_id = parser.message_id(&parsed).map(String::from);
                                        input_tokens = parser.input_tokens_from_start(&parsed).unwrap_or(input_tokens);
                                        cache_creation_tokens = parser.cache_creation_tokens_from_start(&parsed).unwrap_or(cache_creation_tokens);
                                        cache_read_tokens = parser.cache_read_tokens_from_start(&parsed).unwrap_or(cache_read_tokens);
                                    }
                                    Some("message_delta") => {
                                        if let Some(value) = parser.output_tokens_from_delta(&parsed) {
                                            output_tokens = value;
                                        }
                                        if let Some(reason) = parser.stop_reason(&parsed) {
                                            stop_reason = Some(reason.to_string());
                                        }
                                    }
                                    Some("content_block_delta") => {
                                        if let Some(text) = parser.delta_text(&parsed) {
                                            content_text.push_str(text);
                                            normalized.text.push(text.to_string());
                                        }
                                    }
                                    Some("error") => {
                                        if let Some(err) = parsed.get("error") {
                                            error = err.get("message").and_then(|v| v.as_str()).map(String::from);
                                        }
                                    }
                                    _ => {}
                                }
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
        drop(chunk_tx);

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
            model,
            duration_ms: start.elapsed().as_millis() as u64,
            ttft_ms,
            error,
        };
        let _ = meta_tx.send(meta);
    });

    StreamingResponse {
        status_code,
        response_headers: resp_headers_for_return,
        body,
        metadata: meta_rx,
    }
}

/// Parse non-streaming response.
pub async fn handle_non_streaming_response(
    response: reqwest::Response,
    start: Instant,
    protocol: ApiProtocol,
) -> UpstreamResponse {
    let status_code = response.status().as_u16();
    let response_headers = response.headers().clone();

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
                model: None,
                duration_ms: start.elapsed().as_millis() as u64,
                ttft_ms: None,
                error: Some(format!("Failed to read response body: {}", e)),
            }
        }
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
                model: None,
                duration_ms: start.elapsed().as_millis() as u64,
                ttft_ms: None,
                error: if is_http_error {
                    Some(format!("HTTP {}: {}", status_code, body_text.trim()))
                } else {
                    None
                },
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

    let model = body_json
        .get("model")
        .and_then(|v| v.as_str())
        .map(String::from);

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
        model,
        duration_ms: start.elapsed().as_millis() as u64,
        ttft_ms: None,
        error,
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

fn parse_codex_stream_event(
    event: &serde_json::Value,
    normalized: &mut NormalizedResponse,
    input_tokens: &mut u32,
    output_tokens: &mut u32,
    cache_read_tokens: &mut u32,
    message_id: &mut Option<String>,
    model: &mut Option<String>,
) {
    match event.get("type").and_then(|v| v.as_str()) {
        Some("response.output_text.delta") => push_string(event.get("delta"), &mut normalized.text),
        Some("response.reasoning_summary_text.delta") => {
            push_string(event.get("delta"), &mut normalized.thinking)
        }
        Some("response.completed") | Some("response.created") => {
            let response = event.get("response").unwrap_or(event);
            let (input, output, cached) = usage_from_json(response);
            *input_tokens = input.max(*input_tokens);
            *output_tokens = output.max(*output_tokens);
            *cache_read_tokens = cached.max(*cache_read_tokens);
            *message_id = response
                .get("id")
                .and_then(|v| v.as_str())
                .map(String::from);
            *model = response
                .get("model")
                .and_then(|v| v.as_str())
                .map(String::from);
        }
        _ => {}
    }
}

fn push_string(value: Option<&serde_json::Value>, target: &mut Vec<String>) {
    if let Some(value) = value.and_then(|v| v.as_str()).filter(|v| !v.is_empty()) {
        target.push(value.to_string());
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

/// Append effort beta header if effort is active.
pub fn append_effort_beta_header(headers: &mut Vec<(&str, String)>, effort: &str) {
    if effort.is_empty() || effort == "auto" {
        return;
    }
    headers.push(("anthropic-beta", "effort-2025-11-24".to_string()));
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let mut normalized = NormalizedResponse::default();
        let mut input = 0;
        let mut output = 0;
        let mut cached = 0;
        let mut id = None;
        let mut model = None;
        parse_codex_stream_event(
            &serde_json::json!({"type":"response.output_text.delta","delta":"hi"}),
            &mut normalized,
            &mut input,
            &mut output,
            &mut cached,
            &mut id,
            &mut model,
        );
        parse_codex_stream_event(
            &serde_json::json!({
                "type":"response.completed",
                "response":{"id":"resp_1","model":"gpt-5","usage":{
                    "input_tokens":9,"output_tokens":4,
                    "input_tokens_details":{"cached_tokens":3}
                }}
            }),
            &mut normalized,
            &mut input,
            &mut output,
            &mut cached,
            &mut id,
            &mut model,
        );
        assert_eq!(normalized.text, ["hi"]);
        assert_eq!((input, output, cached), (9, 4, 3));
        assert_eq!(id.as_deref(), Some("resp_1"));
    }
}
