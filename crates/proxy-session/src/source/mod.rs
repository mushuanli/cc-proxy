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

pub use anthropic::AnthropicParser;
pub use codex::CodexParser;
pub use heuristic::HeuristicClassifier;
pub use hook::HookParser;
pub use otel::OtelParser;

use proxy_common::ClientType;
use proxy_common::NormalizedResponse;
use serde_json::Value;

use crate::ingest::observation::Observation;
use crate::SessionResult;

/// Facts extracted from a client request body.
#[derive(Debug, Clone, Default)]
pub struct RequestFacts {
    pub prompt_text: Option<String>,
    pub prompt_type: Option<String>,
    pub requested_model: Option<String>,
    pub is_tool_result_last: bool,
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
