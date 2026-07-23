use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use proxy_common::{CostData, DailyCost, ModelCost, ProviderCost, SessionCost};
use proxy_store::DailyUsageRow;
use serde::Deserialize;
use serde_json::json;

use crate::AppState;

#[derive(Deserialize, Default)]
pub struct CostQuery {
    pub from: Option<String>,
    pub to: Option<String>,
}

pub async fn get_costs(
    State(state): State<Arc<AppState>>,
    Query(q): Query<CostQuery>,
) -> impl IntoResponse {
    let from = q.from.unwrap_or_default();
    let to = q.to.unwrap_or_default();

    if from.is_empty() || to.is_empty() {
        return Json(json!({
            "from": "", "to": "",
            "by_model": [], "by_provider": [], "by_session": [], "by_day": []
        }))
        .into_response();
    }

    match state.store.query_daily_usage_range(&from, &to).await {
        Ok(rows) => {
            let data = aggregate_costs(&rows, &from, &to);
            Json(json!(data)).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

fn aggregate_costs(rows: &[DailyUsageRow], from: &str, to: &str) -> CostData {
    // by_model: aggregate across all sessions
    let mut model_map: HashMap<String, ModelCost> = HashMap::new();
    // by_provider: aggregate across all sessions
    let mut provider_map: HashMap<String, ProviderCost> = HashMap::new();
    // by_session: group by session_id
    let mut session_map: HashMap<String, SessionAgg> = HashMap::new();
    // by_day: keep raw rows with date grouping only (already per-day)
    let mut day_map: HashMap<(String, String), DailyCost> = HashMap::new();

    for row in rows {
        // by_model
        let entry = model_map.entry(row.model.clone()).or_insert(ModelCost {
            model: row.model.clone(),
            input_tokens: 0,
            output_tokens: 0,
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            request_count: 0,
        });
        entry.input_tokens += row.input_tokens;
        entry.output_tokens += row.output_tokens;
        entry.cache_creation_tokens += row.cache_creation_tokens;
        entry.cache_read_tokens += row.cache_read_tokens;
        entry.request_count += row.task_count;

        // by_provider
        let entry = provider_map
            .entry(row.provider.clone())
            .or_insert(ProviderCost {
                provider: row.provider.clone(),
                input_tokens: 0,
                output_tokens: 0,
                cache_creation_tokens: 0,
                cache_read_tokens: 0,
                request_count: 0,
            });
        entry.input_tokens += row.input_tokens;
        entry.output_tokens += row.output_tokens;
        entry.cache_creation_tokens += row.cache_creation_tokens;
        entry.cache_read_tokens += row.cache_read_tokens;
        entry.request_count += row.task_count;

        // by_session
        let entry = session_map
            .entry(row.session_id.clone())
            .or_insert(SessionAgg {
                input_tokens: 0,
                output_tokens: 0,
                cache_creation_tokens: 0,
                cache_read_tokens: 0,
                request_count: 0,
                models: Vec::new(),
            });
        entry.input_tokens += row.input_tokens;
        entry.output_tokens += row.output_tokens;
        entry.cache_creation_tokens += row.cache_creation_tokens;
        entry.cache_read_tokens += row.cache_read_tokens;
        entry.request_count += row.task_count;
        if !entry.models.contains(&row.model) {
            entry.models.push(row.model.clone());
        }

        // by_day (one row per date+model+provider, merge to date+model)
        let day_key = (row.usage_date.clone(), row.model.clone());
        let entry = day_map.entry(day_key).or_insert(DailyCost {
            date: row.usage_date.clone(),
            model: row.model.clone(),
            input_tokens: 0,
            output_tokens: 0,
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            request_count: 0,
        });
        entry.input_tokens += row.input_tokens;
        entry.output_tokens += row.output_tokens;
        entry.cache_creation_tokens += row.cache_creation_tokens;
        entry.cache_read_tokens += row.cache_read_tokens;
        entry.request_count += row.task_count;
    }

    let mut by_model: Vec<ModelCost> = model_map.into_values().collect();
    by_model.sort_by_key(|b| std::cmp::Reverse(b.input_tokens));

    let mut by_provider: Vec<ProviderCost> = provider_map.into_values().collect();
    by_provider.sort_by_key(|b| std::cmp::Reverse(b.input_tokens));

    let by_session: Vec<SessionCost> = session_map
        .into_iter()
        .map(|(sid, agg)| SessionCost {
            session_id: sid.clone(),
            session_label: sid,
            input_tokens: agg.input_tokens,
            output_tokens: agg.output_tokens,
            cache_creation_tokens: agg.cache_creation_tokens,
            cache_read_tokens: agg.cache_read_tokens,
            request_count: agg.request_count,
            first_request: String::new(),
            last_request: String::new(),
            models: agg.models,
        })
        .collect();

    let mut by_day: Vec<DailyCost> = day_map.into_values().collect();
    by_day.sort_by(|a, b| a.date.cmp(&b.date));

    CostData {
        from: from.to_string(),
        to: to.to_string(),
        by_model,
        by_provider,
        by_session,
        by_day,
    }
}

struct SessionAgg {
    input_tokens: u64,
    output_tokens: u64,
    cache_creation_tokens: u64,
    cache_read_tokens: u64,
    request_count: u64,
    models: Vec<String>,
}
