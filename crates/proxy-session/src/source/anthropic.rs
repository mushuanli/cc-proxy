//! Claude Code (Anthropic messages API) parser.

use proxy_common::ClientType;
use proxy_common::SseEvent;
use serde_json::Value;

use super::{ClientParser, ParseContext, RequestFacts};
use crate::ingest::observation::{Observation, ObservationKind};

/// Parses Anthropic `messages` format requests and SSE streams.
#[derive(Debug, Default)]
pub struct AnthropicParser;

impl ClientParser for AnthropicParser {
    fn client_type(&self) -> ClientType {
        ClientType::ClaudeCode
    }

    fn parse_request(&self, body: &Value) -> RequestFacts {
        let messages = body
            .get("messages")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        let last = messages.last();
        let is_tool_result_last = last.map(is_tool_result).unwrap_or(false);

        // Extract the latest real user prompt (mirrors store analyzer).
        let prompt_text = messages.iter().rev().find_map(|m| {
            let is_user = m.get("role").and_then(Value::as_str) == Some("user");
            if !is_user || is_tool_result(m) || !is_real_user_prompt(m) {
                return None;
            }
            extract_user_text(m)
        });

        RequestFacts {
            prompt_text: prompt_text.map(|t| truncate(&t, 1000)),
            prompt_type: Some(if is_tool_result_last {
                "continuation".into()
            } else {
                "user".into()
            }),
            requested_model: body.get("model").and_then(Value::as_str).map(String::from),
            is_tool_result_last,
        }
    }

    fn parse_sse(&self, ev: &SseEvent, ctx: &ParseContext) -> Vec<Observation> {
        let Some(data) = ev.data.as_deref() else {
            return Vec::new();
        };
        let parsed: Value = match serde_json::from_str(data) {
            Ok(v) => v,
            Err(_) => return Vec::new(),
        };
        match parsed.get("type").and_then(Value::as_str) {
            Some("content_block_start") => parse_tool_use_start(&parsed, ctx),
            Some("content_block_delta") => parse_input_json_delta(&parsed),
            Some("content_block_stop") => parse_tool_use_stop(&parsed),
            _ => Vec::new(),
        }
    }

    fn parse_response(&self, normalized: &proxy_common::NormalizedResponse, ctx: &ParseContext) -> Vec<Observation> {
        // Non-streaming fallback: tool_calls carried on the normalized response.
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
}

fn parse_tool_use_start(parsed: &Value, ctx: &ParseContext) -> Vec<Observation> {
    let Some(block) = parsed.get("content_block") else {
        return Vec::new();
    };
    if block.get("type").and_then(Value::as_str) != Some("tool_use") {
        return Vec::new();
    }
    let (Some(tool_use_id), Some(tool_name)) = (
        block.get("id").and_then(Value::as_str),
        block.get("name").and_then(Value::as_str),
    ) else {
        return Vec::new();
    };
    let now = chrono::Utc::now().timestamp_millis();
    vec![Observation {
        event_id: format!("tool-emit-{tool_use_id}"),
        session_id: ctx.session_id.clone(),
        source: ctx.source.to_string(),
        occurred_at: now,
        received_at: now,
        source_sequence: None,
        source_version: None,
        payload_hash: format!("tool-emit-{tool_use_id}"),
        kind: ObservationKind::ToolEmitted {
            call_id: ctx.call_id.clone(),
            tool_use_id: tool_use_id.to_string(),
            tool_name: tool_name.to_string(),
            started_at: now,
        },
    }]
}

fn parse_input_json_delta(parsed: &Value) -> Vec<Observation> {
    let Some(delta) = parsed.get("delta") else {
        return Vec::new();
    };
    if delta.get("type").and_then(Value::as_str) != Some("input_json_delta") {
        return Vec::new();
    }
    // The relay SSE loop coalesces deltas into ToolInputComplete; per-delta
    // observations carry the block index as a correlation hint only.
    let tool_use_id = parsed
        .get("index")
        .and_then(Value::as_u64)
        .map(|i| format!("block-{i}"))
        .unwrap_or_default();
    let partial_json = delta
        .get("partial_json")
        .and_then(Value::as_str)
        .unwrap_or_default();
    vec![Observation {
        event_id: format!("tool-delta-{tool_use_id}-{}", partial_json.len()),
        session_id: String::new(),
        source: "proxy".into(),
        occurred_at: 0,
        received_at: 0,
        source_sequence: None,
        source_version: None,
        payload_hash: String::new(),
        kind: ObservationKind::ToolInputDelta {
            tool_use_id,
            partial_json: partial_json.to_string(),
        },
    }]
}

fn parse_tool_use_stop(_parsed: &Value) -> Vec<Observation> {
    // content_block_stop only carries the index; the input accumulation is
    // handled by the relay SSE loop which coalesces deltas before emitting
    // ToolInputComplete. This parser emits nothing for stop.
    Vec::new()
}

fn is_tool_result(msg: &Value) -> bool {
    msg.get("content")
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .any(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
        })
        .unwrap_or(false)
}

fn is_real_user_prompt(msg: &Value) -> bool {
    let content = match msg.get("content").and_then(Value::as_array) {
        Some(c) => c,
        None => return false,
    };
    let all_text = content
        .iter()
        .all(|b| matches!(b.get("type").and_then(Value::as_str), Some("text" | "input_text")));
    if !all_text {
        return false;
    }
    let text = content
        .iter()
        .filter_map(|b| b.get("text").and_then(Value::as_str))
        .filter(|t| !t.trim_start().starts_with("<system-reminder>"))
        .collect::<Vec<_>>()
        .join("\n");
    !text.trim().is_empty()
}

fn extract_user_text(msg: &Value) -> Option<String> {
    let blocks = msg.get("content").and_then(Value::as_array)?;
    let text = blocks
        .iter()
        .filter_map(|b| {
            if matches!(b.get("type").and_then(Value::as_str), Some("text" | "input_text")) {
                b.get("text").and_then(Value::as_str)
            } else {
                None
            }
        })
        .filter(|t| !t.trim_start().starts_with("<system-reminder>"))
        .collect::<Vec<_>>()
        .join("\n");
    (!text.trim().is_empty()).then_some(text)
}

fn truncate(s: &str, max_chars: usize) -> String {
    let mut chars = s.chars();
    let preview: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{preview}…")
    } else {
        preview
    }
}
