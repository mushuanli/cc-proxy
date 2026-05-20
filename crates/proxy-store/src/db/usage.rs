use proxy_common::SessionId;
use rusqlite::{params, Connection};

use crate::error::StoreResult;

/// Upsert daily usage for a task write.
pub fn upsert_daily_usage(
    conn: &Connection,
    session_id: &SessionId,
    provider: &str,
    model: &str,
    currency: &str,
    status: &str,
    input_tokens: u64,
    output_tokens: u64,
    cache_creation_tokens: u64,
    cache_read_tokens: u64,
    cost_microusd: i64,
) -> StoreResult<()> {
    let usage_date = chrono::Utc::now().format("%Y-%m-%d").to_string();

    conn.execute(
        "INSERT INTO session_daily_usage (
            usage_date, session_id, provider, model, currency,
            task_count, completed_task_count, failed_task_count,
            input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens,
            cost_microusd
        ) VALUES (
            ?1, ?2, ?3, ?4, ?5,
            1,
            CASE WHEN ?6 = 'completed' THEN 1 ELSE 0 END,
            CASE WHEN ?6 = 'failed' THEN 1 ELSE 0 END,
            ?7, ?8, ?9, ?10,
            ?11
        )
        ON CONFLICT (usage_date, session_id, provider, model, currency)
        DO UPDATE SET
            task_count = task_count + excluded.task_count,
            completed_task_count = completed_task_count + excluded.completed_task_count,
            failed_task_count = failed_task_count + excluded.failed_task_count,
            input_tokens = input_tokens + excluded.input_tokens,
            output_tokens = output_tokens + excluded.output_tokens,
            cache_creation_tokens = cache_creation_tokens + excluded.cache_creation_tokens,
            cache_read_tokens = cache_read_tokens + excluded.cache_read_tokens,
            cost_microusd = cost_microusd + excluded.cost_microusd",
        params![
            usage_date,
            session_id.as_str(),
            provider,
            model,
            currency,
            status,
            input_tokens as i64,
            output_tokens as i64,
            cache_creation_tokens as i64,
            cache_read_tokens as i64,
            cost_microusd,
        ],
    )?;

    Ok(())
}

/// Query daily usage for a session.
#[derive(Debug, Clone)]
pub struct DailyUsageRow {
    pub usage_date: String,
    pub session_id: String,
    pub provider: String,
    pub model: String,
    pub currency: String,
    pub task_count: u64,
    pub completed_task_count: u64,
    pub failed_task_count: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cache_read_tokens: u64,
    pub cost_microusd: i64,
}

/// Get daily usage for a session.
pub fn get_session_daily_usage(
    conn: &Connection,
    session_id: &SessionId,
) -> StoreResult<Vec<DailyUsageRow>> {
    let mut stmt = conn.prepare(
        "SELECT usage_date, session_id, provider, model, currency,
         task_count, completed_task_count, failed_task_count,
         input_tokens, output_tokens,
         cache_creation_tokens, cache_read_tokens, cost_microusd
         FROM session_daily_usage
         WHERE session_id = ?1
         ORDER BY usage_date DESC, provider, model",
    )?;

    let rows = stmt.query_map(params![session_id.as_str()], |row| {
        Ok(DailyUsageRow {
            usage_date: row.get("usage_date")?,
            session_id: row.get("session_id")?,
            provider: row.get("provider")?,
            model: row.get("model")?,
            currency: row.get("currency")?,
            task_count: row.get::<_, i64>("task_count")? as u64,
            completed_task_count: row.get::<_, i64>("completed_task_count")? as u64,
            failed_task_count: row.get::<_, i64>("failed_task_count")? as u64,
            input_tokens: row.get::<_, i64>("input_tokens")? as u64,
            output_tokens: row.get::<_, i64>("output_tokens")? as u64,
            cache_creation_tokens: row.get::<_, i64>("cache_creation_tokens")? as u64,
            cache_read_tokens: row.get::<_, i64>("cache_read_tokens")? as u64,
            cost_microusd: row.get("cost_microusd")?,
        })
    })?;

    let mut result = Vec::new();
    for row in rows {
        result.push(row?);
    }
    Ok(result)
}

/// Query daily usage for a date range (inclusive).
pub fn query_range(
    conn: &Connection,
    from: &str,
    to: &str,
) -> StoreResult<Vec<DailyUsageRow>> {
    let mut stmt = conn.prepare(
        "SELECT usage_date, session_id, provider, model, currency,
         task_count, completed_task_count, failed_task_count,
         input_tokens, output_tokens,
         cache_creation_tokens, cache_read_tokens, cost_microusd
         FROM session_daily_usage
         WHERE usage_date >= ?1 AND usage_date <= ?2
         ORDER BY usage_date, session_id, provider, model",
    )?;

    let rows = stmt.query_map(params![from, to], |row| {
        Ok(DailyUsageRow {
            usage_date: row.get("usage_date")?,
            session_id: row.get("session_id")?,
            provider: row.get("provider")?,
            model: row.get("model")?,
            currency: row.get("currency")?,
            task_count: row.get::<_, i64>("task_count")? as u64,
            completed_task_count: row.get::<_, i64>("completed_task_count")? as u64,
            failed_task_count: row.get::<_, i64>("failed_task_count")? as u64,
            input_tokens: row.get::<_, i64>("input_tokens")? as u64,
            output_tokens: row.get::<_, i64>("output_tokens")? as u64,
            cache_creation_tokens: row.get::<_, i64>("cache_creation_tokens")? as u64,
            cache_read_tokens: row.get::<_, i64>("cache_read_tokens")? as u64,
            cost_microusd: row.get("cost_microusd")?,
        })
    })?;

    let mut result = Vec::new();
    for row in rows {
        result.push(row?);
    }
    Ok(result)
}
