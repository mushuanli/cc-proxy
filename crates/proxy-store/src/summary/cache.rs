use proxy_common::{SessionId, TaskId, TaskSummary};
use rusqlite::Connection;

use crate::db::{sessions, tasks};
use crate::error::StoreResult;

/// Get or generate a task summary.
///
/// If summary_json is already cached, return it directly.
/// Otherwise, the caller is expected to analyze and cache it.
pub fn get_summary(conn: &Connection, task_id: &TaskId) -> StoreResult<Option<TaskSummary>> {
    let task = match tasks::get_task(conn, task_id)? {
        Some(t) => t,
        None => return Ok(None),
    };

    if let Some(ref json) = task.summary_json {
        if let Ok(summary) = serde_json::from_str::<TaskSummary>(json) {
            return Ok(Some(summary));
        }
    }

    Ok(None)
}

/// Cache a generated summary for a task.
pub fn cache_summary(
    conn: &Connection,
    task_id: &TaskId,
    session_id: &SessionId,
    summary: &TaskSummary,
) -> StoreResult<()> {
    let json = serde_json::to_string(summary)?;
    tasks::update_summary(conn, task_id, &json)?;
    sessions::set_archive_dirty(conn, session_id)?;
    Ok(())
}
