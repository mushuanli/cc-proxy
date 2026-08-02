//! ToolInvocation: one tool call emitted in a model call's response.

use serde::{Deserialize, Serialize};

use super::status::ToolStatus;

/// Row projection of a tool invocation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolInvocationRow {
    pub id: String,
    pub model_call_id: String,
    pub tool_use_id: Option<String>,
    pub operation_seq: i64,
    pub tool_name: String,
    pub status: ToolStatus,
    pub started_at: Option<i64>,
    pub ended_at: Option<i64>,
    pub duration_ms: Option<i64>,
    pub model_input_preview: Option<String>,
    pub effective_input_preview: Option<String>,
    pub raw_result_preview: Option<String>,
    pub effective_result_preview: Option<String>,
}

impl ToolInvocationRow {
    pub fn from_row(row: &rusqlite::Row) -> rusqlite::Result<Self> {
        let status: String = row.get("status")?;
        Ok(Self {
            id: row.get("id")?,
            model_call_id: row.get("model_call_id")?,
            tool_use_id: row.get("tool_use_id")?,
            operation_seq: row.get("operation_seq")?,
            tool_name: row.get("tool_name")?,
            status: ToolStatus::from(status.as_str()),
            started_at: row.get("started_at")?,
            ended_at: row.get("ended_at")?,
            duration_ms: row.get("duration_ms")?,
            model_input_preview: row.get("model_input_preview")?,
            effective_input_preview: row.get("effective_input_preview")?,
            raw_result_preview: row.get("raw_result_preview")?,
            effective_result_preview: row.get("effective_result_preview")?,
        })
    }
}
