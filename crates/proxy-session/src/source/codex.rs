//! Codex client parser (OpenAI Responses API).
//!
//! Codex streams events named `response.*`. Tool calls arrive as:
//!   response.function_call                    → name + call_id + arguments (may be empty)
//!   response.function_call_arguments.delta    → incremental partial arguments (call_id-keyed)
//!   response.function_call_arguments.done     → arguments complete

use proxy_common::ClientType;
use proxy_common::SseEvent;
use serde_json::Value;

use super::{
    obs_from_kind, stream_error, tool_calls_to_observations, ClientParser, ParseContext,
    RequestFacts, StreamUpdate,
};
use crate::ingest::observation::{Observation, ObservationKind};

/// Parses Codex `input` array requests and `response.*` SSE events.
#[derive(Debug, Default)]
pub struct CodexParser {
    /// In-flight function calls: (call_id, name, accumulated arguments).
    calls: Vec<(String, String, String)>,
    /// Index into `calls` of the currently accumulating call.
    pending: Option<usize>,
}

impl ClientParser for CodexParser {
    fn client_type(&self) -> ClientType {
        ClientType::Codex
    }

    fn parse_request(&self, body: &Value) -> RequestFacts {
        let messages = body
            .get("input")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let last = messages.last();
        let is_tool_result_last = last.map(is_tool_result).unwrap_or(false);
        let prompt_text = messages.iter().rev().find_map(|m| {
            let is_user = m.get("role").and_then(Value::as_str) == Some("user");
            if !is_user || is_tool_result(m) {
                return None;
            }
            extract_input_text(m)
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
        let Ok(parsed) = serde_json::from_str::<Value>(data) else {
            return Vec::new();
        };
        let mut parser = CodexParser::default();
        parser.feed_sse(&parsed, ctx).observations
    }

    fn parse_response(&self, normalized: &proxy_common::NormalizedResponse, ctx: &ParseContext) -> Vec<Observation> {
        tool_calls_to_observations(normalized, ctx)
    }

    fn feed_sse(&mut self, ev: &Value, ctx: &ParseContext) -> StreamUpdate {
        match ev.get("type").and_then(Value::as_str) {
            Some("response.output_text.delta") => StreamUpdate {
                text: ev.get("delta").and_then(Value::as_str).map(String::from),
                ..Default::default()
            },
            Some("response.reasoning_summary_text.delta") => StreamUpdate {
                thinking: ev.get("delta").and_then(Value::as_str).map(String::from),
                ..Default::default()
            },
            Some("response.function_call") => self.on_function_call(ev, ctx),
            Some("response.function_call_arguments.delta") => self.on_args_delta(ev),
            Some("response.function_call_arguments.done") => self.on_args_done(ev, ctx),
            Some("response.completed") | Some("response.created") => self.on_completed(ev),
            Some("error") | Some("response.failed") => {
                stream_error(ev.get("response").unwrap_or(ev))
            }
            _ => StreamUpdate::default(),
        }
    }

    fn finish_stream(&mut self, ctx: &ParseContext) -> Vec<Observation> {
        let Some(i) = self.pending.take() else {
            return Vec::new();
        };
        if let Some((call_id, _, args)) = self.calls.get_mut(i) {
            return vec![obs_from_kind(
                ObservationKind::ToolInputComplete {
                    tool_use_id: std::mem::take(call_id),
                    input_json: std::mem::take(args),
                },
                ctx,
            )];
        }
        Vec::new()
    }
}

impl CodexParser {
    fn on_function_call(&mut self, ev: &Value, ctx: &ParseContext) -> StreamUpdate {
        let Some(call_id) = ev.get("call_id").and_then(Value::as_str) else {
            return StreamUpdate::default();
        };
        let name = ev.get("name").and_then(Value::as_str).unwrap_or("tool");
        let arguments = ev.get("arguments").and_then(Value::as_str).unwrap_or("");
        self.calls
            .push((call_id.to_string(), name.to_string(), arguments.to_string()));
        let idx = self.calls.len() - 1;
        self.pending = Some(idx);
        StreamUpdate {
            observations: vec![obs_from_kind(
                ObservationKind::ToolEmitted {
                    call_id: ctx.call_id.clone(),
                    tool_use_id: call_id.to_string(),
                    tool_name: name.to_string(),
                    started_at: chrono::Utc::now().timestamp_millis(),
                },
                ctx,
            )],
            ..Default::default()
        }
    }

    fn on_args_delta(&mut self, ev: &Value) -> StreamUpdate {
        let Some(i) = self.pending else {
            return StreamUpdate::default();
        };
        if let Some((_, _, acc)) = self.calls.get_mut(i) {
            if let Some(delta) = ev.get("delta").and_then(Value::as_str) {
                acc.push_str(delta);
            }
        }
        StreamUpdate::default()
    }

    fn on_args_done(&mut self, _ev: &Value, ctx: &ParseContext) -> StreamUpdate {
        let Some(i) = self.pending.take() else {
            return StreamUpdate::default();
        };
        let Some((call_id, _, args)) = self.calls.get_mut(i) else {
            return StreamUpdate::default();
        };
        StreamUpdate {
            observations: vec![obs_from_kind(
                ObservationKind::ToolInputComplete {
                    tool_use_id: std::mem::take(call_id),
                    input_json: std::mem::take(args),
                },
                ctx,
            )],
            ..Default::default()
        }
    }

    fn on_completed(&self, ev: &Value) -> StreamUpdate {
        let response = ev.get("response").unwrap_or(ev);
        let usage = response.get("usage").unwrap_or(&Value::Null);
        let cached = usage
            .get("input_tokens_details")
            .and_then(|v| v.get("cached_tokens"))
            .and_then(Value::as_u64);
        StreamUpdate {
            input_tokens: usage.get("input_tokens").and_then(Value::as_u64).map(super::u64_to_u32),
            output_tokens: usage.get("output_tokens").and_then(Value::as_u64).map(super::u64_to_u32),
            cache_read_tokens: cached.map(super::u64_to_u32),
            message_id: response.get("id").and_then(Value::as_str).map(String::from),
            model: response.get("model").and_then(Value::as_str).map(String::from),
            ..Default::default()
        }
    }
}

fn is_tool_result(msg: &Value) -> bool {
    msg.get("content")
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .any(|b| b.get("type").and_then(Value::as_str) == Some("function_call_output"))
        })
        .unwrap_or(false)
}

fn extract_input_text(msg: &Value) -> Option<String> {
    let blocks = msg.get("content").and_then(Value::as_array)?;
    let text = blocks
        .iter()
        .filter_map(|b| b.get("input_text").and_then(Value::as_str))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> ParseContext {
        ParseContext {
            call_id: "call-1".into(),
            session_id: "sess-1".into(),
            source: "proxy",
        }
    }

    #[test]
    fn function_call_stream_accumulates_arguments() {
        let mut parser = CodexParser::default();
        let c = ctx();

        // Emit tool_use.
        let u = parser.feed_sse(
            &serde_json::json!({
                "type": "response.function_call",
                "call_id": "fc_1",
                "name": "Bash",
                "arguments": ""
            }),
            &c,
        );
        assert_eq!(u.observations.len(), 1);
        assert!(matches!(&u.observations[0].kind,
            ObservationKind::ToolEmitted { tool_use_id, tool_name, .. }
            if tool_use_id == "fc_1" && tool_name == "Bash"));

        // Accumulate two argument deltas.
        parser.feed_sse(
            &serde_json::json!({"type": "response.function_call_arguments.delta", "call_id": "fc_1", "delta": "{\"cmd\":"}),
            &c,
        );
        parser.feed_sse(
            &serde_json::json!({"type": "response.function_call_arguments.delta", "call_id": "fc_1", "delta": "\"ls\"}"}),
            &c,
        );

        // Complete → ToolInputComplete with concatenated args.
        let done = parser.feed_sse(
            &serde_json::json!({"type": "response.function_call_arguments.done", "call_id": "fc_1"}),
            &c,
        );
        assert_eq!(done.observations.len(), 1);
        assert!(matches!(&done.observations[0].kind,
            ObservationKind::ToolInputComplete { tool_use_id, input_json }
            if tool_use_id == "fc_1" && input_json == r#"{"cmd":"ls"}"#));
    }

    #[test]
    fn parse_response_emits_tool_calls() {
        let parser = CodexParser::default();
        let normalized = proxy_common::NormalizedResponse {
            tool_calls: vec![proxy_common::ToolCallRecord {
                id: "fc_9".into(),
                name: "Read".into(),
                input: serde_json::json!({"file_path": "a.rs"}),
            }],
            ..Default::default()
        };
        let obs = parser.parse_response(&normalized, &ctx());
        assert_eq!(obs.len(), 1);
        assert!(matches!(&obs[0].kind,
            ObservationKind::ToolEmitted { tool_use_id, tool_name, .. }
            if tool_use_id == "fc_9" && tool_name == "Read"));
    }

    #[test]
    fn extract_tool_results_handles_function_call_output() {
        let body = r#"{
            "input": [
                {"role": "user", "content": [{"type": "function_call_output", "call_id": "fc_1", "output": "ok"}]}
            ]
        }"#;
        let obs = crate::extract_tool_results(body, "sess-1");
        assert_eq!(obs.len(), 1);
        assert!(matches!(&obs[0].kind,
            ObservationKind::ToolResult { tool_use_id, status, .. }
            if tool_use_id == "fc_1" && status == "succeeded"));
    }
}
