//! Observation: an append-only raw event fed into the reconciler.

use serde::{Deserialize, Serialize};

/// Token usage snapshot for a finished model call.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cache_read_tokens: u64,
}

/// Typed payload of an observation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ObservationKind {
    ModelCallStart {
        call_id: String,
        client_request_id: Option<String>,
        requested_model: Option<String>,
        started_at: i64,
    },
    ModelCallFirstToken {
        call_id: String,
        ttft_ms: u64,
    },
    ToolEmitted {
        call_id: String,
        tool_use_id: String,
        tool_name: String,
        started_at: i64,
    },
    ToolInputDelta {
        tool_use_id: String,
        partial_json: String,
    },
    ToolInputComplete {
        tool_use_id: String,
        input_json: String,
    },
    ToolResult {
        tool_use_id: String,
        raw_result_preview: Option<String>,
        effective_result_preview: Option<String>,
        status: String,
    },
    ModelCallEnd {
        call_id: String,
        status: String,
        tokens: TokenUsage,
        stop_reason: Option<String>,
        cost_microusd: i64,
        ended_at: i64,
        provider_request_id: Option<String>,
        error: Option<String>,
        http_status_code: Option<u16>,
    },
    PromptSubmit {
        prompt_id: String,
        prompt_text: Option<String>,
        started_at: i64,
    },
    AgentStart {
        agent_id: String,
        agent_type: String,
        started_at: i64,
    },
    AgentStop {
        agent_id: String,
        ended_at: i64,
    },
}

/// Raw event appended to the `observations` table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Observation {
    pub event_id: String,
    pub session_id: String,
    pub source: String,
    pub occurred_at: i64,
    pub received_at: i64,
    pub source_sequence: Option<String>,
    pub source_version: Option<String>,
    pub payload_hash: String,
    pub kind: ObservationKind,
}

impl Observation {
    /// Human-readable event type derived from the payload kind.
    pub fn event_type(&self) -> &'static str {
        match &self.kind {
            ObservationKind::ModelCallStart { .. } => "model_call_start",
            ObservationKind::ModelCallFirstToken { .. } => "model_call_first_token",
            ObservationKind::ToolEmitted { .. } => "tool_emitted",
            ObservationKind::ToolInputDelta { .. } => "tool_input_delta",
            ObservationKind::ToolInputComplete { .. } => "tool_input_complete",
            ObservationKind::ToolResult { .. } => "tool_result",
            ObservationKind::ModelCallEnd { .. } => "model_call_end",
            ObservationKind::PromptSubmit { .. } => "prompt_submit",
            ObservationKind::AgentStart { .. } => "agent_start",
            ObservationKind::AgentStop { .. } => "agent_stop",
        }
    }

    /// Correlation hint for the reconciler (call_id / tool_use_id / prompt_id / agent_id).
    pub fn correlation_hint(&self) -> Option<&str> {
        match &self.kind {
            ObservationKind::ModelCallStart { call_id, .. }
            | ObservationKind::ModelCallFirstToken { call_id, .. }
            | ObservationKind::ModelCallEnd { call_id, .. } => Some(call_id.as_str()),
            ObservationKind::ToolEmitted { tool_use_id, .. }
            | ObservationKind::ToolInputDelta { tool_use_id, .. }
            | ObservationKind::ToolInputComplete { tool_use_id, .. }
            | ObservationKind::ToolResult { tool_use_id, .. } => Some(tool_use_id.as_str()),
            ObservationKind::PromptSubmit { prompt_id, .. } => Some(prompt_id.as_str()),
            ObservationKind::AgentStart { agent_id, .. } | ObservationKind::AgentStop { agent_id, .. } => {
                Some(agent_id.as_str())
            }
        }
    }
}
