//! Upstream dispatch utilities: header filtering, SSE parsing, retry logic.
//!
//! Extracted from proxy-server's proxy.rs to keep the relay handler focused.

use std::collections::HashMap;
use std::time::Instant;

use axum::http::{HeaderMap, HeaderValue};
use bytes::Bytes;
use proxy_common::models::SseEvent;
use crate::sse::SseParser;

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
            "host" | "connection" | "transfer-encoding"
                | "content-length" | "accept-encoding"
                | "proxy-connection" | "proxy-authorization"
        ) {
            continue;
        }
        if override_token.is_some() && (key == "authorization" || key == "x-api-key") {
            continue;
        }
        fwd.insert(k.clone(), v.clone());
    }
    fwd.insert("accept-encoding", HeaderValue::from_static("identity"));

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
    fwd
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

// ── Response from upstream dispatch ──

/// Result of dispatching a request upstream.
pub struct UpstreamResponse {
    pub status_code: u16,
    pub response_headers: HeaderMap,
    pub content_text: Option<String>,
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
            .post(url)
            .headers(headers.clone())
            .body(body.clone())
            .timeout(std::time::Duration::from_secs(timeout_secs.max(1)))
            .build()
        {
            Ok(r) => r,
            Err(e) => {
                last_err = e.to_string();
                continue;
            }
        };

        match client.execute(req).await {
            Ok(resp) => return Ok(resp),
            Err(e) => {
                last_err = e.to_string();
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
) -> UpstreamResponse {
    use futures::StreamExt;

    let status_code = response.status().as_u16();
    let response_headers = response.headers().clone();
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

    let mut parser = SseParser::new();
    let mut byte_stream = response.bytes_stream();

    loop {
        match byte_stream.next().await {
            Some(Ok(chunk)) => {
                if ttft_ms.is_none() {
                    ttft_ms = Some(start.elapsed().as_millis() as u64);
                }

                let events = parser.feed(&chunk);
                for ev in &events {
                    sse_events.push(ev.clone());

                    if let Some(data_str) = &ev.data {
                        if let Some(parsed) = parser.parse_message_data(data_str) {
                            match parser.event_kind(&parsed) {
                                Some("message_start") => {
                                    if let Some(msg) = parsed.get("message") {
                                        if let Some(m) = msg.get("model").and_then(|v| v.as_str()) {
                                            model = Some(m.to_string());
                                        }
                                        if let Some(id) = msg.get("id").and_then(|v| v.as_str()) {
                                            message_id = Some(id.to_string());
                                        }
                                        if let Some(usage) = msg.get("usage") {
                                            input_tokens = usage
                                                .get("input_tokens")
                                                .and_then(|v| v.as_u64())
                                                .unwrap_or(0) as u32;
                                            cache_creation_tokens = usage
                                                .get("cache_creation_input_tokens")
                                                .and_then(|v| v.as_u64())
                                                .unwrap_or(0) as u32;
                                            cache_read_tokens = usage
                                                .get("cache_read_input_tokens")
                                                .and_then(|v| v.as_u64())
                                                .unwrap_or(0) as u32;
                                        }
                                    }
                                }
                                Some("message_delta") => {
                                    if let Some(usage) = parsed
                                        .get("delta")
                                        .and_then(|d| d.get("usage"))
                                    {
                                        output_tokens = usage
                                            .get("output_tokens")
                                            .and_then(|v| v.as_u64())
                                            .unwrap_or(0) as u32;
                                    }
                                    if let Some(delta) = parsed.get("delta") {
                                        if let Some(reason) =
                                            delta.get("stop_reason").and_then(|v| v.as_str())
                                        {
                                            stop_reason = Some(reason.to_string());
                                        }
                                    }
                                }
                                Some("content_block_delta") => {
                                    if let Some(delta) = parsed.get("delta") {
                                        if let Some(text) =
                                            delta.get("text").and_then(|v| v.as_str())
                                        {
                                            content_text.push_str(text);
                                        } else if let Some(thinking) =
                                            delta.get("thinking").and_then(|v| v.as_str())
                                        {
                                            content_text.push_str(thinking);
                                        }
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

    UpstreamResponse {
        status_code,
        response_headers,
        content_text: if content_text.is_empty() {
            None
        } else {
            Some(content_text)
        },
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

/// Parse non-streaming response.
pub async fn handle_non_streaming_response(
    response: reqwest::Response,
    start: Instant,
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
            }
        }
    };

    let input_tokens = body_json
        .get("usage")
        .and_then(|u| u.get("input_tokens"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32;

    let output_tokens = body_json
        .get("usage")
        .and_then(|u| u.get("output_tokens"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32;

    let cache_creation_tokens = body_json
        .get("usage")
        .and_then(|u| u.get("cache_creation_input_tokens"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32;

    let cache_read_tokens = body_json
        .get("usage")
        .and_then(|u| u.get("cache_read_input_tokens"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32;

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

    let content_text = body_json
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
    headers.push(("anthropic-beta", format!("effort-2025-11-24")));
}
