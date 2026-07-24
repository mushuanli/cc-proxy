use proxy_common::{SessionId, TaskId};
use serde::{Deserialize, Serialize};

use crate::db::usage::DailyUsageRow;
use crate::models::{Session, Task};
use crate::summary::analyzer::SessionSummary;

/// Top-level archive document written to YAML.
#[derive(Debug, Serialize, Deserialize)]
pub struct ArchiveDocument {
    pub version: u32,
    pub session: ArchiveSession,
    pub statistics: ArchiveStatistics,
    pub tasks: Vec<ArchiveTask>,
    pub daily_usage: Vec<ArchiveDailyUsage>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ArchiveSession {
    pub id: SessionId,
    pub name: Option<String>,
    pub client_type: String,
    pub client_session_id: Option<String>,
    pub cwd: Option<String>,
    pub project_key: Option<String>,
    pub created_at: i64,
    pub last_activity_at: i64,
    pub status: String,
    pub ended_at: Option<i64>,
    pub latest_provider: Option<String>,
    pub latest_model: Option<String>,
    pub latest_upstream: Option<String>,
    pub last_task_id: Option<TaskId>,
    pub last_task_status: Option<String>,
    pub last_stop_reason: Option<String>,
    pub last_error_type: Option<String>,
    pub last_error_message: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ArchiveStatistics {
    pub task_count: u64,
    pub completed_task_count: u64,
    pub failed_task_count: u64,

    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cache_read_tokens: u64,

    pub cost_microusd: i64,
    pub currency: String,
    pub priced_task_count: u64,
    pub unpriced_task_count: u64,
    pub total_duration_ms: i64,
    pub total_ttft_ms: i64,
    pub ttft_task_count: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ArchiveTask {
    pub id: TaskId,
    pub sequence_no: u64,

    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub status: String,

    pub provider: String,
    pub pricing_model_id: Option<String>,
    pub requested_model: Option<String>,
    pub resolved_model: String,
    pub upstream: Option<String>,

    pub pricing: ArchivePricing,
    pub usage: ArchiveUsage,
    pub timing: ArchiveTiming,
    pub error: Option<ArchiveError>,
    pub prompt: Option<String>,
    pub summary: Option<SessionSummary>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ArchivePricing {
    pub input_rate_microusd: i64,
    pub output_rate_microusd: i64,
    pub cache_write_rate_microusd: i64,
    pub cache_read_rate_microusd: i64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ArchiveUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cache_read_tokens: u64,
    pub cost_microusd: i64,
    pub currency: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ArchiveTiming {
    pub duration_ms: Option<i64>,
    pub ttft_ms: Option<i64>,
    pub stop_reason: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ArchiveError {
    pub error_type: String,
    pub message: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ArchiveDailyUsage {
    pub date: String,
    pub provider: String,
    pub model: String,
    pub task_count: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_microusd: i64,
}

/// Build a readable summary document without raw request messages.
pub fn build_archive(
    session: &Session,
    tasks: &[Task],
    daily_usage: &[DailyUsageRow],
) -> ArchiveDocument {
    ArchiveDocument {
        version: 3,
        session: build_session(session),
        statistics: build_statistics(session),
        tasks: tasks.iter().map(build_task_summary).collect(),
        daily_usage: daily_usage.iter().map(build_daily_usage).collect(),
    }
}

fn build_statistics(session: &Session) -> ArchiveStatistics {
    ArchiveStatistics {
        task_count: session.task_count,
        completed_task_count: session.completed_task_count,
        failed_task_count: session.failed_task_count,
        input_tokens: session.total_input_tokens,
        output_tokens: session.total_output_tokens,
        cache_creation_tokens: session.total_cache_creation_tokens,
        cache_read_tokens: session.total_cache_read_tokens,
        cost_microusd: session.total_cost_microusd,
        currency: session.currency.clone(),
        priced_task_count: session.priced_task_count,
        unpriced_task_count: session.unpriced_task_count,
        total_duration_ms: session.total_duration_ms,
        total_ttft_ms: session.total_ttft_ms,
        ttft_task_count: session.ttft_task_count,
    }
}

fn build_session(session: &Session) -> ArchiveSession {
    ArchiveSession {
        id: session.id.clone(),
        name: session.name.clone(),
        client_type: session.client_type.as_str().to_string(),
        client_session_id: session.client_session_id.clone(),
        cwd: session.cwd.clone(),
        project_key: session.project_key.clone(),
        created_at: session.created_at,
        last_activity_at: session.last_activity_at,
        status: session.status.clone(),
        ended_at: session.ended_at,
        latest_provider: session.latest_provider.clone(),
        latest_model: session.latest_model.clone(),
        latest_upstream: session.latest_upstream.clone(),
        last_task_id: session.last_task_id.clone(),
        last_task_status: session.last_task_status.clone(),
        last_stop_reason: session.last_stop_reason.clone(),
        last_error_type: session.last_error_type.clone(),
        last_error_message: session.last_error_message.clone(),
    }
}

fn build_daily_usage(usage: &DailyUsageRow) -> ArchiveDailyUsage {
    ArchiveDailyUsage {
        date: usage.usage_date.clone(),
        provider: usage.provider.clone(),
        model: usage.model.clone(),
        task_count: usage.task_count,
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        cost_microusd: usage.cost_microusd,
    }
}

fn build_task_summary(task: &Task) -> ArchiveTask {
    ArchiveTask {
        id: task.id.clone(),
        sequence_no: task.sequence_no,
        started_at: task.started_at,
        ended_at: task.ended_at,
        status: task.status.as_str().to_string(),
        provider: task.provider.clone(),
        pricing_model_id: task.pricing_model_id.clone(),
        requested_model: task.requested_model.clone(),
        resolved_model: task.resolved_model.clone(),
        upstream: task.upstream.clone(),
        pricing: build_pricing(task),
        usage: build_usage(task),
        timing: build_timing(task),
        error: build_error(task),
        prompt: task.prompt_text.clone(),
        summary: task
            .summary_json
            .as_ref()
            .and_then(|json| serde_json::from_str(json).ok()),
    }
}

fn build_pricing(task: &Task) -> ArchivePricing {
    ArchivePricing {
        input_rate_microusd: task.input_rate_microusd,
        output_rate_microusd: task.output_rate_microusd,
        cache_write_rate_microusd: task.cache_write_rate_microusd,
        cache_read_rate_microusd: task.cache_read_rate_microusd,
    }
}

fn build_usage(task: &Task) -> ArchiveUsage {
    ArchiveUsage {
        input_tokens: task.input_tokens,
        output_tokens: task.output_tokens,
        cache_creation_tokens: task.cache_creation_tokens,
        cache_read_tokens: task.cache_read_tokens,
        cost_microusd: task.cost_microusd,
        currency: task.currency.clone(),
    }
}

fn build_timing(task: &Task) -> ArchiveTiming {
    ArchiveTiming {
        duration_ms: task.duration_ms,
        ttft_ms: task.ttft_ms,
        stop_reason: task.stop_reason.clone(),
    }
}

fn build_error(task: &Task) -> Option<ArchiveError> {
    let error_type = task
        .error_type
        .as_deref()
        .filter(|value| !value.is_empty())?;
    Some(ArchiveError {
        error_type: error_type.to_string(),
        message: task.error_message.clone().unwrap_or_default(),
    })
}
