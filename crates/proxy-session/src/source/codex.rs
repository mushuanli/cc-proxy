//! Codex client parser (OpenAI-style `input` array, `function_call_arguments` deltas).

use proxy_common::ClientType;
use proxy_common::SseEvent;
use serde_json::Value;

use super::{ClientParser, ParseContext, RequestFacts};
use crate::ingest::observation::Observation;

/// Parses Codex `input` array requests and function-call SSE deltas.
#[derive(Debug, Default)]
pub struct CodexParser;

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
        // Codex streams function_call_arguments via `response.function_call_arguments.delta`
        // which is a single JSON object, not block-indexed deltas. P1 expands this.
        let _ = ev;
        let _ = ctx;
        Vec::new()
    }

    fn parse_response(&self, _normalized: &proxy_common::NormalizedResponse, _ctx: &ParseContext) -> Vec<Observation> {
        Vec::new()
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
