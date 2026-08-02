//! Status enums for the session domain model.

use serde::{Deserialize, Serialize};

/// Lifecycle status of a model call (one logical HTTP request).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CallStatus {
    InProgress,
    Completed,
    Failed,
    Cancelled,
    Interrupted,
}

impl CallStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Interrupted => "interrupted",
        }
    }
}

impl From<&str> for CallStatus {
    fn from(s: &str) -> Self {
        match s {
            "completed" => Self::Completed,
            "failed" => Self::Failed,
            "cancelled" => Self::Cancelled,
            "interrupted" => Self::Interrupted,
            _ => Self::InProgress,
        }
    }
}

/// Lifecycle status of a tool invocation (one tool call).
///
/// `emitted` means the model produced a tool_use intent — NOT that the tool ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    Emitted,
    InputComplete,
    AwaitingPermission,
    Running,
    Succeeded,
    Failed,
    Denied,
    Interrupted,
    Abandoned,
}

impl ToolStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Emitted => "emitted",
            Self::InputComplete => "input_complete",
            Self::AwaitingPermission => "awaiting_permission",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Denied => "denied",
            Self::Interrupted => "interrupted",
            Self::Abandoned => "abandoned",
        }
    }
}

impl From<&str> for ToolStatus {
    fn from(s: &str) -> Self {
        match s {
            "input_complete" => Self::InputComplete,
            "awaiting_permission" => Self::AwaitingPermission,
            "running" => Self::Running,
            "succeeded" => Self::Succeeded,
            "failed" => Self::Failed,
            "denied" => Self::Denied,
            "interrupted" => Self::Interrupted,
            "abandoned" => Self::Abandoned,
            _ => Self::Emitted,
        }
    }
}

/// Classification source and confidence for a derived relation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Classification {
    pub source: String,
    pub confidence: String,
    pub version: String,
}

impl Default for Classification {
    fn default() -> Self {
        Self {
            source: "heuristic".into(),
            confidence: "weak".into(),
            version: "claude-code-v2".into(),
        }
    }
}

/// Rank confidence levels for merge precedence: weak < strong < exact.
pub fn confidence_rank(c: &str) -> u8 {
    match c {
        "exact" => 3,
        "strong" => 2,
        _ => 1,
    }
}
