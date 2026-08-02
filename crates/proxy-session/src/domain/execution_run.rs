//! ExecutionRun: one continuous execution (main turn, subagent, title, compact, memory, recap).

use serde::{Deserialize, Serialize};

/// A run kind distinguishes user-initiated work from internal system runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunKind {
    Main,
    Subagent,
    Compact,
    Title,
    Memory,
    Recap,
    System,
}

impl RunKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Main => "main",
            Self::Subagent => "subagent",
            Self::Compact => "compact",
            Self::Title => "title",
            Self::Memory => "memory",
            Self::Recap => "recap",
            Self::System => "system",
        }
    }
}

impl From<&str> for RunKind {
    fn from(s: &str) -> Self {
        match s {
            "subagent" => Self::Subagent,
            "compact" => Self::Compact,
            "title" => Self::Title,
            "memory" => Self::Memory,
            "recap" => Self::Recap,
            "system" => Self::System,
            _ => Self::Main,
        }
    }
}

/// Row projection of an execution run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionRunRow {
    pub id: String,
    pub session_id: String,
    pub interaction_id: Option<String>,
    pub run_kind: RunKind,
    pub agent_run_id: Option<String>,
    pub started_at: i64,
    pub foreground_completed_at: Option<i64>,
    pub settled_at: Option<i64>,
    pub status: String,
    pub classification_source: String,
    pub classification_confidence: String,
}
