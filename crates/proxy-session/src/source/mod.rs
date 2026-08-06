//! ClientParsers: client-specific session semantics.
//!
//! This is the extension point for supporting other client session formats
//! (Claude Code, Codex, future clients). Each parser translates a client's
//! request/SSE/response into normalized `Observation`s.

pub mod anthropic;
pub mod codex;
pub mod heuristic;
pub mod hook;
pub mod otel;
pub mod stream;

pub use anthropic::AnthropicParser;
pub use codex::CodexParser;
pub use heuristic::HeuristicClassifier;
pub use hook::HookParser;
pub use otel::OtelParser;
pub use stream::ToolStreamParser;

use proxy_common::ClientType;
use proxy_common::NormalizedResponse;
use serde_json::Value;

use crate::ingest::observation::{Observation, ObservationKind};
use crate::SessionResult;

/// Facts extracted from a client request body.
#[derive(Debug, Clone, Default)]
pub struct RequestFacts {
    pub prompt_text: Option<String>,
    pub prompt_type: Option<String>,
    pub requested_model: Option<String>,
    pub is_tool_result_last: bool,
}

/// Incremental metadata + observations produced by one streamed SSE event.
#[derive(Debug, Clone, Default)]
pub struct StreamUpdate {
    pub text: Option<String>,
    pub thinking: Option<String>,
    pub input_tokens: Option<u32>,
    pub output_tokens: Option<u32>,
    pub cache_creation_tokens: Option<u32>,
    pub cache_read_tokens: Option<u32>,
    pub stop_reason: Option<String>,
    pub message_id: Option<String>,
    pub model: Option<String>,
    pub error: Option<String>,
    pub observations: Vec<Observation>,
}

/// A parser translating one client's protocol into normalized observations.
pub trait ClientParser: Send + Sync {
    fn client_type(&self) -> ClientType;

    /// Parse a request body into request facts.
    fn parse_request(&self, body: &Value) -> RequestFacts;

    /// Translate one SSE event into zero or more observations.
    fn parse_sse(&self, ev: &proxy_common::SseEvent, context: &ParseContext) -> Vec<Observation>;

    /// Fallback for non-streaming responses: extract tool calls from the response body.
    fn parse_response(&self, normalized: &NormalizedResponse, context: &ParseContext) -> Vec<Observation>;

    /// Stateful stream parser: feed one parsed SSE JSON, return incremental
    /// metadata + observations. The default is stateless (no-op).
    fn feed_sse(&mut self, _ev: &Value, _context: &ParseContext) -> StreamUpdate {
        StreamUpdate::default()
    }

    /// Stream-end fallback: return closing observations for any in-flight
    /// tool use (e.g. abandoned). Default is empty.
    fn finish_stream(&mut self, _context: &ParseContext) -> Vec<Observation> {
        Vec::new()
    }
}

/// Context shared across a single model call while parsing.
#[derive(Debug, Clone)]
pub struct ParseContext {
    pub call_id: String,
    pub session_id: String,
    pub source: &'static str,
}

impl Default for ParseContext {
    fn default() -> Self {
        Self {
            call_id: String::new(),
            session_id: String::new(),
            source: "proxy",
        }
    }
}

/// Validate that a parser produces only observations it owns.
pub fn validate_observations(obs: &[Observation]) -> SessionResult<()> {
    for o in obs {
        if o.session_id.is_empty() {
            return Err(crate::SessionError::InvalidArgument(
                "observation missing session_id".into(),
            ));
        }
    }
    Ok(())
}

// ── Shared helpers for client parsers ──

/// Wrap a typed observation kind into a full `Observation` with context fields.
///
/// Also backfills the call_id inside `ToolEmitted` from the context. The
/// stateful `ToolStreamParser` emits these with an empty call_id (it does not
/// own the current model call); without this fix the tool invocation rows are
/// written with `model_call_id=""` and never attach to their model call.
pub fn obs_from_kind(kind: ObservationKind, ctx: &ParseContext) -> Observation {
    let now = chrono::Utc::now().timestamp_millis();
    // Include a kind discriminator so multiple observations produced within
    // the same millisecond (e.g. a codex function_call emit + args done) do
    // not collide on the idempotency key.
    let disc = kind_discriminator(&kind);
    let event_id = format!("{}-{now}-{disc}", ctx.call_id);
    let kind = match kind {
        ObservationKind::ToolEmitted {
            tool_use_id,
            tool_name,
            started_at,
            ..
        } => ObservationKind::ToolEmitted {
            call_id: ctx.call_id.clone(),
            tool_use_id,
            tool_name,
            started_at,
        },
        other => other,
    };
    Observation {
        event_id: event_id.clone(),
        session_id: ctx.session_id.clone(),
        source: ctx.source.to_string(),
        occurred_at: now,
        received_at: now,
        source_sequence: None,
        source_version: None,
        payload_hash: event_id,
        kind,
    }
}

fn kind_discriminator(kind: &ObservationKind) -> String {
    match kind {
        ObservationKind::ToolEmitted { tool_use_id, .. } => format!("emit-{tool_use_id}"),
        ObservationKind::ToolInputComplete { tool_use_id, .. } => format!("input-{tool_use_id}"),
        ObservationKind::ToolResult { tool_use_id, .. } => format!("result-{tool_use_id}"),
        ObservationKind::ToolInputDelta { tool_use_id, .. } => format!("td-{tool_use_id}"),
        ObservationKind::ModelCallStart { call_id, .. } => format!("start-{call_id}"),
        ObservationKind::ModelCallEnd { call_id, .. } => format!("end-{call_id}"),
        ObservationKind::ModelCallFirstToken { call_id, .. } => format!("tft-{call_id}"),
        ObservationKind::PromptSubmit { prompt_id, .. } => format!("prompt-{prompt_id}"),
        ObservationKind::AgentStart { agent_id, .. } => format!("astart-{agent_id}"),
        ObservationKind::AgentStop { agent_id, .. } => format!("astop-{agent_id}"),
    }
}

/// Build a stream error update from an SSE error payload.
pub fn stream_error(ev: &Value) -> StreamUpdate {
    StreamUpdate {
        error: ev
            .get("error")
            .and_then(|v| v.get("message"))
            .and_then(Value::as_str)
            .map(String::from),
        ..Default::default()
    }
}

/// Translate non-streaming tool calls into ToolEmitted observations.
pub fn tool_calls_to_observations(
    normalized: &NormalizedResponse,
    ctx: &ParseContext,
) -> Vec<Observation> {
    let now = chrono::Utc::now().timestamp_millis();
    normalized
        .tool_calls
        .iter()
        .map(|tool| Observation {
            event_id: format!("resp-{}-{}", ctx.call_id, tool.id),
            session_id: ctx.session_id.clone(),
            source: ctx.source.to_string(),
            occurred_at: now,
            received_at: now,
            source_sequence: None,
            source_version: None,
            payload_hash: format!("resp-{}-{}", ctx.call_id, tool.id),
            kind: ObservationKind::ToolEmitted {
                call_id: ctx.call_id.clone(),
                tool_use_id: tool.id.clone(),
                tool_name: tool.name.clone(),
                started_at: now,
            },
        })
        .collect()
}

pub fn u64_to_u32(v: u64) -> u32 {
    v.min(u32::MAX as u64) as u32
}

/// Extract tool_result blocks from a request body into ToolResult observations.
///
/// Supports both Anthropic (`messages`/`tool_result`/`tool_use_id`/`content`)
/// and Codex (`input`/`function_call_output`/`call_id`/`output`) formats.
pub fn extract_tool_results(request_body: &str, session_id: &str) -> Vec<Observation> {
    let Ok(value) = serde_json::from_str::<Value>(request_body) else {
        return Vec::new();
    };
    let mut out = extract_result_blocks(
        &value,
        "messages",
        "tool_result",
        "tool_use_id",
        "content",
        session_id,
    );
    out.extend(extract_result_blocks(
        &value,
        "input",
        "function_call_output",
        "call_id",
        "output",
        session_id,
    ));
    out
}

#[allow(clippy::too_many_arguments)]
fn extract_result_blocks(
    value: &Value,
    array_key: &str,
    block_type: &str,
    id_key: &str,
    content_key: &str,
    session_id: &str,
) -> Vec<Observation> {
    let Some(messages) = value.get(array_key).and_then(Value::as_array) else {
        return Vec::new();
    };
    let now = chrono::Utc::now().timestamp_millis();
    let mut out = Vec::new();
    for msg in messages {
        if msg.get("role").and_then(Value::as_str) != Some("user") {
            continue;
        }
        let Some(blocks) = msg.get("content").and_then(Value::as_array) else {
            continue;
        };
        for block in blocks {
            if block.get("type").and_then(Value::as_str) != Some(block_type) {
                continue;
            }
            let Some(tool_use_id) = block.get(id_key).and_then(Value::as_str) else {
                continue;
            };
            let preview = match block.get(content_key) {
                Some(Value::Array(items)) => items
                    .iter()
                    .filter_map(|i| i.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("\n"),
                Some(Value::String(s)) => s.clone(),
                _ => String::new(),
            };
            let status = if block.get("is_error").and_then(Value::as_bool) == Some(true) {
                "failed"
            } else {
                "succeeded"
            };
            let event_id = format!("tool-result-{tool_use_id}");
            out.push(Observation {
                event_id: event_id.clone(),
                session_id: session_id.to_string(),
                source: "proxy".into(),
                occurred_at: now,
                received_at: now,
                source_sequence: None,
                source_version: None,
                payload_hash: event_id,
                kind: ObservationKind::ToolResult {
                    tool_use_id: tool_use_id.to_string(),
                    raw_result_preview: Some(truncate(&preview, 500)),
                    effective_result_preview: None,
                    status: status.into(),
                },
            });
        }
    }
    out
}

pub fn truncate(s: &str, max_chars: usize) -> String {
    let mut chars = s.chars();
    let preview: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{preview}…")
    } else {
        preview
    }
}
