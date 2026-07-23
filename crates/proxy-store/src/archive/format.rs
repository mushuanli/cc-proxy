use proxy_common::{SessionId, TaskId};
use serde::{Deserialize, Serialize};

use crate::db::usage::DailyUsageRow;
use crate::models::{Session, Task};

/// Top-level archive document written to YAML.
#[derive(Debug, Serialize, Deserialize)]
pub struct ArchiveDocument {
    pub version: u32,
    pub session: ArchiveSession,
    pub statistics: ArchiveStatistics,
    pub latest_task: Option<ArchiveTask>,
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

    pub pricing: ArchivePricing,
    pub usage: ArchiveUsage,

    pub request: ArchiveRequest,
    pub response: Option<ArchiveResponse>,
    pub summary: Option<ArchiveSummary>,
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
pub struct ArchiveRequest {
    pub method: String,
    pub path: String,
    pub body: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ArchiveResponse {
    pub body: Option<serde_json::Value>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ArchiveSummary {
    pub user_request: Option<String>,
    pub assistant_result: Option<String>,
    pub touched_files: Vec<String>,
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

/// Build an ArchiveDocument from session, latest task, and daily usage.
pub fn build_archive(
    session: &Session,
    latest_task: Option<&Task>,
    daily_usage: &[DailyUsageRow],
) -> ArchiveDocument {
    let statistics = ArchiveStatistics {
        task_count: session.task_count,
        completed_task_count: session.completed_task_count,
        failed_task_count: session.failed_task_count,
        input_tokens: session.total_input_tokens,
        output_tokens: session.total_output_tokens,
        cache_creation_tokens: session.total_cache_creation_tokens,
        cache_read_tokens: session.total_cache_read_tokens,
        cost_microusd: session.total_cost_microusd,
        currency: session.currency.clone(),
    };

    let latest = latest_task.map(|t| ArchiveTask {
        id: t.id.clone(),
        sequence_no: t.sequence_no,
        started_at: t.started_at,
        ended_at: t.ended_at,
        status: t.status.as_str().to_string(),
        provider: t.provider.clone(),
        pricing_model_id: t.pricing_model_id.clone(),
        requested_model: t.requested_model.clone(),
        resolved_model: t.resolved_model.clone(),
        pricing: ArchivePricing {
            input_rate_microusd: t.input_rate_microusd,
            output_rate_microusd: t.output_rate_microusd,
            cache_write_rate_microusd: t.cache_write_rate_microusd,
            cache_read_rate_microusd: t.cache_read_rate_microusd,
        },
        usage: ArchiveUsage {
            input_tokens: t.input_tokens,
            output_tokens: t.output_tokens,
            cache_creation_tokens: t.cache_creation_tokens,
            cache_read_tokens: t.cache_read_tokens,
            cost_microusd: t.cost_microusd,
            currency: t.currency.clone(),
        },
        request: ArchiveRequest {
            method: t.method.clone(),
            path: t.path.clone(),
            body: t.request_body.clone(),
        },
        response: t.response_body.as_ref().map(|body| ArchiveResponse {
            body: Some(serde_json::to_value(body).unwrap_or_default()),
        }),
        summary: t.summary_json.as_ref().and_then(|s| {
            serde_json::from_str::<serde_json::Value>(s)
                .ok()
                .map(|v| ArchiveSummary {
                    user_request: v
                        .get("user_request")
                        .and_then(|u| u.as_str())
                        .map(String::from),
                    assistant_result: v
                        .get("assistant_result")
                        .and_then(|a| a.as_str())
                        .map(String::from),
                    touched_files: v
                        .get("touched_files")
                        .and_then(|f| f.as_array())
                        .map(|a| {
                            a.iter()
                                .filter_map(|x| x.as_str().map(String::from))
                                .collect()
                        })
                        .unwrap_or_default(),
                })
        }),
    });

    let daily: Vec<ArchiveDailyUsage> = daily_usage
        .iter()
        .map(|d| ArchiveDailyUsage {
            date: d.usage_date.clone(),
            provider: d.provider.clone(),
            model: d.model.clone(),
            task_count: d.task_count,
            input_tokens: d.input_tokens,
            output_tokens: d.output_tokens,
            cost_microusd: d.cost_microusd,
        })
        .collect();

    ArchiveDocument {
        version: 1,
        session: ArchiveSession {
            id: session.id.clone(),
            name: session.name.clone(),
            client_type: session.client_type.as_str().to_string(),
            client_session_id: session.client_session_id.clone(),
            cwd: session.cwd.clone(),
            project_key: session.project_key.clone(),
            created_at: session.created_at,
            last_activity_at: session.last_activity_at,
        },
        statistics,
        latest_task: latest,
        daily_usage: daily,
    }
}
