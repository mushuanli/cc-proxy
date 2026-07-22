use chrono::{DateTime, Utc};
use proxy_common::{BillingSnapshot, ClientType, NormalizedResponse, SessionId, TaskId, TaskStatus, TaskUsage};
use serde::{Deserialize, Serialize};

/// A Session as stored in the database.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Session {
    pub id: SessionId,
    pub client_type: ClientType,
    pub client_session_id: Option<String>,
    pub name: Option<String>,
    pub cwd: Option<String>,
    pub project_key: Option<String>,

    pub created_at: i64,
    pub first_activity_at: i64,
    pub last_activity_at: i64,

    pub task_count: u64,
    pub completed_task_count: u64,
    pub failed_task_count: u64,

    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub total_cache_creation_tokens: u64,
    pub total_cache_read_tokens: u64,

    pub total_cost_microusd: i64,
    pub currency: String,

    pub next_task_sequence: u64,
    pub last_archived_at: Option<i64>,
    pub last_archived_task_id: Option<TaskId>,
    pub last_archived_sequence: u64,
    pub archive_dirty: bool,

    pub metadata: serde_json::Value,
}

/// A Task as stored in the database.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Task {
    pub id: TaskId,
    pub session_id: SessionId,
    pub sequence_no: u64,

    pub created_at: i64,
    pub started_at: i64,
    pub first_byte_at: Option<i64>,
    pub ended_at: Option<i64>,

    pub status: TaskStatus,

    pub method: String,
    pub path: String,

    pub request_headers: Option<serde_json::Value>,
    pub request_body: Option<String>,

    pub response_headers: Option<serde_json::Value>,
    pub response_body: Option<NormalizedResponse>,

    pub http_status_code: Option<u16>,
    pub is_streaming: bool,

    pub requested_model: Option<String>,
    pub provider: String,
    pub pricing_model_id: Option<String>,
    pub resolved_model: String,
    pub upstream: Option<String>,

    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cache_read_tokens: u64,

    pub duration_ms: Option<i64>,
    pub ttft_ms: Option<i64>,

    pub stop_reason: Option<String>,
    pub upstream_message_id: Option<String>,

    pub error_type: Option<String>,
    pub error_message: Option<String>,

    pub input_rate_microusd: i64,
    pub output_rate_microusd: i64,
    pub cache_write_rate_microusd: i64,
    pub cache_read_rate_microusd: i64,

    pub cost_microusd: i64,
    pub currency: String,

    pub summary_json: Option<String>,
    pub summary_created_at: Option<i64>,

    pub metadata: serde_json::Value,

    /// Number of messages in the request body (for display in list views).
    pub messages_count: u32,
}

/// Input to write a new task.
#[derive(Clone, Debug)]
pub struct NewTask {
    pub id: Option<TaskId>,
    pub session_defaults: NewSessionDefaults,

    pub started_at: i64,
    pub first_byte_at: Option<i64>,
    pub ended_at: Option<i64>,

    pub status: TaskStatus,

    pub method: String,
    pub path: String,

    pub request_headers: Option<serde_json::Value>,
    pub request_body: Option<String>,

    pub response_headers: Option<serde_json::Value>,
    pub response_body: Option<NormalizedResponse>,

    pub http_status_code: Option<u16>,
    pub is_streaming: bool,

    pub requested_model: Option<String>,
    pub upstream: Option<String>,

    pub billing: BillingSnapshot,
    pub usage: TaskUsage,

    pub timing: TaskTiming,
    pub error: Option<TaskError>,

    pub metadata: serde_json::Value,

    /// Number of messages in the request body (for display in list views).
    pub messages_count: u32,
}

/// Defaults for creating a new session.
#[derive(Clone, Debug, Default)]
pub struct NewSessionDefaults {
    pub client_type: ClientType,
    pub client_session_id: Option<String>,
    pub name: Option<String>,
    pub cwd: Option<String>,
    pub project_key: Option<String>,
}

/// Timing information for a task.
#[derive(Clone, Debug, Default)]
pub struct TaskTiming {
    pub duration_ms: Option<i64>,
    pub ttft_ms: Option<i64>,
    pub stop_reason: Option<String>,
    pub upstream_message_id: Option<String>,
}

/// Error information for a task.
#[derive(Clone, Debug, Default)]
pub struct TaskError {
    pub error_type: String,
    pub error_message: String,
}

// ── List item types (lightweight, no body) ──

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionListItem {
    pub id: SessionId,
    pub client_type: ClientType,
    pub client_session_id: Option<String>,
    pub name: Option<String>,
    pub cwd: Option<String>,
    pub project_key: Option<String>,
    pub created_at: i64,
    pub last_activity_at: i64,
    pub task_count: u64,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub total_cache_creation_tokens: u64,
    pub total_cache_read_tokens: u64,
    pub total_cost_microusd: i64,
    pub archive_dirty: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TaskListItem {
    pub id: TaskId,
    pub sequence_no: u64,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub status: String,
    pub method: String,
    pub path: String,
    pub provider: String,
    pub resolved_model: String,
    pub http_status_code: Option<u16>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_microusd: i64,
    pub priced: bool,
    pub duration_ms: Option<i64>,
    pub ttft_ms: Option<i64>,
    pub summary_json: Option<String>,
    /// Number of messages in the request body.
    pub messages_count: u32,
}

// ── Filter / options types ──

#[derive(Clone, Debug, Default)]
pub struct SessionFilter {
    pub id_or_name: Option<String>,
    pub client_type: Option<ClientType>,
    pub project_key: Option<String>,
    pub time_range: Option<TimeRange>,
    pub limit: Option<u32>,
    pub offset: Option<u32>,
}

#[derive(Clone, Debug)]
pub struct TimeRange {
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug)]
pub struct ArchiveOptions {
    pub task_retention_hours: u32,
    pub force: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ArchiveInfo {
    pub session_id: SessionId,
    pub name: Option<String>,
    pub file_path: String,
    pub archived_at: Option<i64>,
    pub task_count: u64,
}
