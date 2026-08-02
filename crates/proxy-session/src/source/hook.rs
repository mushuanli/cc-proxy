//! Hook event parser (Claude Code hooks → observations).

use proxy_common::ClientType;
use proxy_common::SseEvent;
use serde_json::Value;

use super::{ClientParser, ParseContext, RequestFacts};
use crate::ingest::observation::{Observation, ObservationKind};

/// Parses hook events delivered via `POST /api/hook-event`.
#[derive(Debug, Default)]
pub struct HookParser;

impl ClientParser for HookParser {
    fn client_type(&self) -> ClientType {
        ClientType::ClaudeCode
    }

    fn parse_request(&self, _body: &Value) -> RequestFacts {
        RequestFacts::default()
    }

    fn parse_sse(&self, _ev: &SseEvent, _ctx: &ParseContext) -> Vec<Observation> {
        Vec::new()
    }

    fn parse_response(&self, _normalized: &proxy_common::NormalizedResponse, _ctx: &ParseContext) -> Vec<Observation> {
        Vec::new()
    }
}

impl HookParser {
    /// Parse a hook event payload into observations.
    pub fn parse_hook_event(
        &self,
        session_id: &str,
        event_name: &str,
        input: &Value,
        hook_input: &Value,
    ) -> Vec<Observation> {
        let now = chrono::Utc::now().timestamp_millis();
        match event_name {
            "UserPromptSubmit" => {
                let prompt_id = hook_input
                    .get("prompt_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let text = hook_input
                    .get("prompt")
                    .and_then(Value::as_str)
                    .map(|s| truncate(s, 1000));
                vec![Observation {
                    event_id: format!("hook-prompt-{session_id}-{now}"),
                    session_id: session_id.to_string(),
                    source: "hook".into(),
                    occurred_at: now,
                    received_at: now,
                    source_sequence: Some(event_name.into()),
                    source_version: hook_input
                        .get("claude_code_version")
                        .and_then(Value::as_str)
                        .map(String::from),
                    payload_hash: format!("hook-prompt-{prompt_id}"),
                    kind: ObservationKind::PromptSubmit {
                        prompt_id: prompt_id.to_string(),
                        prompt_text: text,
                        started_at: now,
                    },
                }]
            }
            "PreToolUse" | "PostToolUse" | "PostToolUseFailure" => {
                let tool_use_id = hook_input
                    .get("tool_use_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let status = if event_name == "PostToolUseFailure" {
                    "failed"
                } else if event_name == "PostToolUse" {
                    "succeeded"
                } else {
                    "running"
                };
                vec![Observation {
                    event_id: format!("hook-tool-{session_id}-{now}"),
                    session_id: session_id.to_string(),
                    source: "hook".into(),
                    occurred_at: now,
                    received_at: now,
                    source_sequence: Some(event_name.into()),
                    source_version: None,
                    payload_hash: format!("hook-tool-{tool_use_id}"),
                    kind: ObservationKind::ToolResult {
                        tool_use_id: tool_use_id.to_string(),
                        raw_result_preview: input
                            .get("tool_input")
                            .and_then(Value::as_str)
                            .map(|s| truncate(s, 500)),
                        effective_result_preview: input
                            .get("tool_response")
                            .and_then(Value::as_str)
                            .map(|s| truncate(s, 500)),
                        status: status.into(),
                    },
                }]
            }
            "SubagentStart" => {
                let agent_id = hook_input
                    .get("agent_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                vec![Observation {
                    event_id: format!("hook-agent-{session_id}-{now}"),
                    session_id: session_id.to_string(),
                    source: "hook".into(),
                    occurred_at: now,
                    received_at: now,
                    source_sequence: Some(event_name.into()),
                    source_version: None,
                    payload_hash: format!("hook-agent-{agent_id}"),
                    kind: ObservationKind::AgentStart {
                        agent_id: agent_id.to_string(),
                        agent_type: hook_input
                            .get("agent_type")
                            .and_then(Value::as_str)
                            .unwrap_or("claude")
                            .to_string(),
                        started_at: now,
                    },
                }]
            }
            "SubagentStop" | "Stop" | "StopFailure" => {
                let agent_id = hook_input
                    .get("agent_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                vec![Observation {
                    event_id: format!("hook-agent-stop-{session_id}-{now}"),
                    session_id: session_id.to_string(),
                    source: "hook".into(),
                    occurred_at: now,
                    received_at: now,
                    source_sequence: Some(event_name.into()),
                    source_version: None,
                    payload_hash: format!("hook-agent-stop-{agent_id}"),
                    kind: ObservationKind::AgentStop {
                        agent_id: agent_id.to_string(),
                        ended_at: now,
                    },
                }]
            }
            _ => Vec::new(),
        }
    }
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
