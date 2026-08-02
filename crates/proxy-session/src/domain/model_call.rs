//! ModelCall: one logical HTTP request from the client to the proxy.

use serde::{Deserialize, Serialize};

use super::status::CallStatus;

/// Row projection of a model call for timeline queries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelCallRow {
    pub id: String,
    pub session_id: String,
    pub sequence_no: i64,
    pub previous_model_call_id: Option<String>,
    pub client_request_id: Option<String>,
    pub provider_request_id: Option<String>,
    pub status: CallStatus,
    pub started_at: i64,
    pub requested_model: Option<String>,
    pub resolved_model: String,
    pub provider: String,
    pub upstream: Option<String>,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_creation_tokens: i64,
    pub cache_read_tokens: i64,
    pub cost_microusd: i64,
    pub duration_ms: Option<i64>,
    pub ttft_ms: Option<i64>,
    pub stop_reason: Option<String>,
    pub http_status_code: Option<i64>,
    pub error_type: Option<String>,
    pub error_message: Option<String>,
}

impl ModelCallRow {
    #[allow(clippy::too_many_arguments)]
    pub fn from_row(row: &rusqlite::Row) -> rusqlite::Result<Self> {
        let status: String = row.get("status")?;
        Ok(Self {
            id: row.get("id")?,
            session_id: row.get("session_id")?,
            sequence_no: row.get("sequence_no")?,
            previous_model_call_id: row.get("previous_model_call_id")?,
            client_request_id: row.get("client_request_id")?,
            provider_request_id: row.get("provider_request_id")?,
            status: CallStatus::from(status.as_str()),
            started_at: row.get("started_at")?,
            requested_model: row.get("requested_model")?,
            resolved_model: row.get("resolved_model")?,
            provider: row.get("provider")?,
            upstream: row.get("upstream")?,
            input_tokens: row.get("input_tokens")?,
            output_tokens: row.get("output_tokens")?,
            cache_creation_tokens: row.get("cache_creation_tokens")?,
            cache_read_tokens: row.get("cache_read_tokens")?,
            cost_microusd: row.get("cost_microusd")?,
            duration_ms: row.get("duration_ms")?,
            ttft_ms: row.get("ttft_ms")?,
            stop_reason: row.get("stop_reason")?,
            http_status_code: row.get("http_status_code")?,
            error_type: row.get("error_type")?,
            error_message: row.get("error_message")?,
        })
    }
}
