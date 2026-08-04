//! Claude Code (Anthropic messages API) parser.

use proxy_common::ClientType;
use proxy_common::SseEvent;
use serde_json::Value;

use super::{
    obs_from_kind, stream_error, tool_calls_to_observations, ClientParser, ParseContext,
    RequestFacts, StreamUpdate,
};
use crate::ingest::observation::{Observation, ObservationKind};

/// Parses Anthropic `messages` format requests and SSE streams.
#[derive(Debug, Default)]
pub struct AnthropicParser {
    tools: super::stream::ToolStreamParser,
}

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
        tool_calls_to_observations(normalized, ctx)
    }

    fn feed_sse(&mut self, parsed: &Value, ctx: &ParseContext) -> StreamUpdate {
        match parsed.get("type").and_then(Value::as_str) {
            Some("message_start") => {
                let message = parsed.get("message").unwrap_or(parsed);
                StreamUpdate {
                    message_id: message.get("id").and_then(Value::as_str).map(String::from),
                    model: message.get("model").and_then(Value::as_str).map(String::from),
                    input_tokens: usage_u64(message, "input_tokens"),
                    cache_creation_tokens: usage_u64(message, "cache_creation_input_tokens"),
                    cache_read_tokens: usage_u64(message, "cache_read_input_tokens"),
                    ..Default::default()
                }
            }
            Some("message_delta") => {
                let delta = parsed.get("delta").unwrap_or(parsed);
                StreamUpdate {
                    output_tokens: usage_u64(parsed, "output_tokens"),
                    stop_reason: delta.get("stop_reason").and_then(Value::as_str).map(String::from),
                    ..Default::default()
                }
            }
            Some("content_block_delta") => {
                let delta = parsed.get("delta").unwrap_or(parsed);
                let mut update = StreamUpdate {
                    text: delta.get("text").and_then(Value::as_str).map(String::from),
                    thinking: delta.get("thinking").and_then(Value::as_str).map(String::from),
                    ..Default::default()
                };
                // Feed tool input_json deltas to the tool stream parser.
                for kind in self.tools.feed(parsed) {
                    update.observations.push(obs_from_kind(kind, ctx));
                }
                update
            }
            Some("content_block_start") | Some("content_block_stop") => {
                let mut update = StreamUpdate::default();
                for kind in self.tools.feed(parsed) {
                    update.observations.push(obs_from_kind(kind, ctx));
                }
                update
            }
            Some("error") => stream_error(parsed),
            _ => StreamUpdate::default(),
        }
    }

    fn finish_stream(&mut self, ctx: &ParseContext) -> Vec<Observation> {
        self.tools
            .finish()
            .into_iter()
            .map(|kind| obs_from_kind(kind, ctx))
            .collect()
    }
}

fn usage_u64(ev: &Value, key: &str) -> Option<u32> {
    ev.get("usage")
        .and_then(|u| u.get(key))
        .and_then(Value::as_u64)
        .map(super::u64_to_u32)
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
    proxy_common::is_tool_result(msg)
}

fn is_real_user_prompt(msg: &Value) -> bool {
    proxy_common::is_real_user_prompt(msg)
}

fn extract_user_text(msg: &Value) -> Option<String> {
    let text = proxy_common::extract_user_text(msg);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_tool_results_with_task_number() {
        let body = r#"{
            "messages": [
                {"role": "user", "content": "hello"},
                {"role": "assistant", "content": [{"type": "tool_use", "id": "call_1", "name": "TaskCreate", "input": {"subject": "修复导出"}}]},
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "call_1", "content": "Task #1 created successfully: 修复导出"}]}
            ]
        }"#;
        let obs = crate::extract_tool_results(body, "sess-1");
        assert_eq!(obs.len(), 1);
        let ObservationKind::ToolResult { tool_use_id, raw_result_preview, status, .. } = &obs[0].kind else {
            panic!("expected ToolResult");
        };
        assert_eq!(tool_use_id, "call_1");
        assert_eq!(raw_result_preview.as_deref(), Some("Task #1 created successfully: 修复导出"));
        assert_eq!(status, "succeeded");
    }

    #[test]
    fn tool_result_error_sets_failed_status() {
        let body = r#"{
            "messages": [
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "call_2", "is_error": true, "content": "boom"}]}
            ]
        }"#;
        let obs = crate::extract_tool_results(body, "sess-1");
        assert_eq!(obs.len(), 1);
        let ObservationKind::ToolResult { status, .. } = &obs[0].kind else { panic!() };
        assert_eq!(status, "failed");
    }
}
