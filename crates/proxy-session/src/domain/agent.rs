//! AgentIdentity + AgentRun: stable agent identity and one run segment.

use serde::{Deserialize, Serialize};

/// Row projection of a stable agent identity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentIdentityRow {
    pub id: String,
    pub session_id: String,
    pub external_agent_id: Option<String>,
    pub agent_type: String,
    pub synthetic: bool,
}

/// Row projection of one agent run segment (a start or resume).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentRunRow {
    pub id: String,
    pub session_id: String,
    pub identity_id: String,
    pub run_no: i64,
    pub parent_agent_run_id: Option<String>,
    pub spawned_by_tool_invocation_id: Option<String>,
    pub interaction_id: Option<String>,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub status: String,
}
