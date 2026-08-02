//! Five independent correlation classes.
//!
//! These solve different problems and must NOT be merged into a single
//! precedence chain. Each maps a set of source IDs to a domain relation.

use crate::domain::status::confidence_rank;

/// Correlation categories, each with its own sources.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CorrelationClass {
    /// Map an HTTP request to its OTel model-call span.
    ModelCall,
    /// Group execution runs under a user prompt.
    PromptGroup,
    /// Group model calls under an agent.
    AgentGroup,
    /// Link tool_use ↔ tool_result ↔ hook ↔ OTel.
    ToolLink,
    /// Child subagent ← parent agent run.
    AgentParent,
}

impl CorrelationClass {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ModelCall => "model_call",
            Self::PromptGroup => "prompt_group",
            Self::AgentGroup => "agent_group",
            Self::ToolLink => "tool_link",
            Self::AgentParent => "agent_parent",
        }
    }
}

/// A candidate relation with a confidence tier.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub class: CorrelationClass,
    pub subject: String,
    pub value: String,
    pub confidence: &'static str,
}

/// Pick the strongest candidate for a subject within a class.
pub fn pick_strongest(candidates: &[Candidate], class: CorrelationClass) -> Option<&Candidate> {
    candidates
        .iter()
        .filter(|c| c.class == class)
        .max_by_key(|c| confidence_rank(c.confidence))
}

/// A weak candidate never overrides an exact one for the same (class, subject).
pub fn can_overwrite(existing_confidence: &str, incoming_confidence: &str) -> bool {
    confidence_rank(incoming_confidence) > confidence_rank(existing_confidence)
}
