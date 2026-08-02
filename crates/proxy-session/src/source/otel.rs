//! OpenTelemetry span parser (traceparent / prompt.id / agent_id).

use proxy_common::ClientType;
use proxy_common::SseEvent;
use serde_json::Value;

use super::{ClientParser, ParseContext, RequestFacts};
use crate::ingest::observation::{Observation, ObservationKind};

/// Parses OTel span events for exact correlation.
///
/// Enabled when the client propagates `traceparent`
/// (`CLAUDE_CODE_PROPAGATE_TRACEPARENT=1`).
#[derive(Debug, Default)]
pub struct OtelParser;

impl ClientParser for OtelParser {
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

impl OtelParser {
    /// Build a model-attempt observation from an OTel span payload.
    pub fn parse_span(&self, session_id: &str, span: &Value) -> Vec<Observation> {
        let now = chrono::Utc::now().timestamp_millis();
        let trace_id = span.get("trace_id").and_then(Value::as_str).unwrap_or_default();
        let span_id = span.get("span_id").and_then(Value::as_str).unwrap_or_default();
        let call_id = span
            .get("client_request_id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if call_id.is_empty() {
            return Vec::new();
        }
        vec![Observation {
            event_id: format!("otel-span-{session_id}-{trace_id}-{span_id}"),
            session_id: session_id.to_string(),
            source: "otel".into(),
            occurred_at: now,
            received_at: now,
            source_sequence: Some(span_id.into()),
            source_version: None,
            payload_hash: format!("otel-span-{trace_id}-{span_id}"),
            kind: ObservationKind::ModelCallStart {
                call_id: call_id.to_string(),
                client_request_id: Some(call_id.to_string()),
                requested_model: span
                    .get("model")
                    .and_then(Value::as_str)
                    .map(String::from),
                prompt_text: span
                    .get("prompt")
                    .and_then(Value::as_str)
                    .map(String::from),
                started_at: now,
            },
        }]
    }
}
