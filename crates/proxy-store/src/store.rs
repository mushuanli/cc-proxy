use proxy_common::{SessionId, TaskId};
use std::path::PathBuf;
use std::sync::Arc;

use crate::archive::manager::ArchiveManager;
use crate::command::{RunCommand, RunResult};
use crate::db::{self, connection, migration};
use crate::error::{StoreError, StoreResult};
use crate::models::{
    ArchiveInfo, ArchiveOptions, ArchiveSearchResult, NewTask, Session, SessionFilter,
    SessionListItem, Task, TaskListItem, TimeRange,
};
use crate::summary::analyzer::SessionSummary;
use rusqlite::Connection;
use std::sync::Mutex;

/// Configuration for opening a ProxyStore.
#[derive(Clone, Debug)]
pub struct ProxyStoreConfig {
    pub database_path: PathBuf,
    pub archive_dir: PathBuf,
    pub busy_timeout_ms: u64,
}

impl Default for ProxyStoreConfig {
    fn default() -> Self {
        Self {
            database_path: PathBuf::from("data/datav2.db"),
            archive_dir: PathBuf::from("data/archives"),
            busy_timeout_ms: 5000,
        }
    }
}

/// The main store: SQLite database + archive files.
///
/// All public I/O methods are async and offload blocking work to
/// `tokio::task::spawn_blocking` so they don't stall Tokio workers.
#[derive(Clone)]
pub struct ProxyStore {
    inner: Arc<StoreInner>,
}

struct StoreInner {
    conn: Mutex<Connection>,
    archive: ArchiveManager,
}

impl ProxyStore {
    /// Open the store, creating directories and running migrations.
    /// Synchronous — only called during startup before the async runtime is busy.
    pub fn open(config: ProxyStoreConfig) -> StoreResult<Self> {
        if let Some(parent) = config.database_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let archive = ArchiveManager::new(config.archive_dir.clone());
        archive.init()?;

        let conn = connection::open_database(&config.database_path)?;
        migration::migrate(&conn)?;

        tracing::info!(
            "proxy-store opened: db={}, archive_dir={}",
            config.database_path.display(),
            config.archive_dir.display()
        );

        Ok(Self {
            inner: Arc::new(StoreInner {
                conn: Mutex::new(conn),
                archive,
            }),
        })
    }

    /// Offload a blocking closure to `spawn_blocking` and await the result.
    async fn blocking<F, T>(f: F) -> StoreResult<T>
    where
        F: FnOnce() -> StoreResult<T> + Send + 'static,
        T: Send + 'static,
    {
        tokio::task::spawn_blocking(f)
            .await
            .map_err(|e| StoreError::InvalidArgument(format!("spawn_blocking panicked: {}", e)))?
    }

    // ── Write ──

    /// Write a new task. Auto-creates the session if it doesn't exist.
    /// Idempotent: if a TaskId is provided and already exists, no aggregates are updated.
    ///
    /// Cost is computed internally from `task.billing.rates` and `task.usage` —
    /// the caller does not need to calculate cost.
    pub async fn task_write(&self, session_id: &SessionId, task: NewTask) -> StoreResult<Task> {
        let sid_short: String = session_id
            .as_str()
            .chars()
            .rev()
            .take(8)
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        let task_id_debug = task
            .id
            .as_ref()
            .map(|id| {
                id.as_str()
                    .chars()
                    .rev()
                    .take(8)
                    .collect::<String>()
                    .chars()
                    .rev()
                    .collect::<String>()
            })
            .unwrap_or_else(|| "new".into());

        let this = self.clone();
        let sid_debug = sid_short;
        let tid_debug = task_id_debug;
        let sid = session_id.clone();

        Self::blocking(move || {
            let conn = this.inner.conn.lock().unwrap();
            conn.execute_batch("BEGIN IMMEDIATE")?;

            let result = (|| -> StoreResult<Task> {
                let cost_microusd =
                    crate::billing::calculate_cost_microusd(&task.usage, &task.billing.rates)
                        .map_err(|e| StoreError::InvalidArgument(e.to_string()))?;

                let task_id = task.id.clone().unwrap_or_else(TaskId::generate);

                if db::tasks::get_task(&conn, &task_id)?.is_some() {
                    return db::tasks::get_task(&conn, &task_id)?
                        .ok_or_else(|| StoreError::NotFound("task vanished".into()));
                }

                let first_activity_at = task.started_at;
                db::sessions::ensure_session(
                    &conn,
                    &sid,
                    &task.session_defaults,
                    first_activity_at,
                )?;

                let sequence_no = db::sessions::allocate_sequence(&conn, &sid)?;
                let inserted = db::tasks::insert_task(
                    &conn,
                    &task,
                    &task_id,
                    &sid,
                    sequence_no,
                    cost_microusd,
                )?;

                if inserted {
                    let priced = task.billing.pricing_model_id != "unknown";
                    let duration_ms = task.timing.duration_ms.unwrap_or(0);
                    db::sessions::update_aggregates(
                        &conn,
                        &sid,
                        task.status.as_str(),
                        task.usage.input_tokens,
                        task.usage.output_tokens,
                        task.usage.cache_creation_tokens,
                        task.usage.cache_read_tokens,
                        cost_microusd,
                        task.started_at,
                        &task.billing.provider,
                        &task.billing.resolved_model,
                        task.upstream.as_deref(),
                        priced,
                        duration_ms,
                        task.timing.ttft_ms,
                        task.ended_at,
                        &task_id,
                        task.timing.stop_reason.as_deref(),
                        task.error.as_ref().map(|e| e.error_type.as_str()),
                        task.error.as_ref().map(|e| e.error_message.as_str()),
                    )?;
                    db::usage::upsert_daily_usage(
                        &conn,
                        &sid,
                        &task.billing.provider,
                        &task.billing.resolved_model,
                        &task.billing.currency,
                        task.status.as_str(),
                        task.usage.input_tokens,
                        task.usage.output_tokens,
                        task.usage.cache_creation_tokens,
                        task.usage.cache_read_tokens,
                        cost_microusd,
                    )?;
                }

                db::tasks::get_task(&conn, &task_id)?
                    .ok_or_else(|| StoreError::NotFound("task not found after write".into()))
            })();

            match result {
                Ok(task) => {
                    conn.execute_batch("COMMIT")?;
                    Ok(task)
                }
                Err(e) => {
                    let _ = conn.execute_batch("ROLLBACK");
                    Err(e)
                }
            }
        })
        .await
        .map_err(|e| {
            tracing::error!(
                "[store] write failed: sid=…{} task={} error={}",
                sid_debug,
                tid_debug,
                e
            );
            e
        })
    }

    // ── Read ──

    /// Get full task details by id.
    pub async fn task_info(&self, task_id: &TaskId) -> StoreResult<Task> {
        let this = self.clone();
        let tid = task_id.clone();
        Self::blocking(move || {
            let conn = this.inner.conn.lock().unwrap();
            db::tasks::get_task(&conn, &tid)?
                .ok_or_else(|| StoreError::NotFound(format!("task '{}' not found", tid)))
        })
        .await
    }

    /// List sessions with optional filters.
    pub async fn session_list(&self, filter: SessionFilter) -> StoreResult<Vec<SessionListItem>> {
        let this = self.clone();
        Self::blocking(move || {
            let conn = this.inner.conn.lock().unwrap();
            db::sessions::list_sessions(&conn, &filter)
        })
        .await
    }

    /// Get a single session by exact ID.
    pub async fn session_get(&self, id: &SessionId) -> StoreResult<Option<Session>> {
        let this = self.clone();
        let sid = id.clone();
        Self::blocking(move || {
            let conn = this.inner.conn.lock().unwrap();
            db::sessions::get_session(&conn, &sid)
        })
        .await
    }

    /// Find the latest recording session — fallback when a request has no session_id.
    pub async fn session_headless(&self) -> StoreResult<Option<SessionId>> {
        let this = self.clone();
        Self::blocking(move || {
            let conn = this.inner.conn.lock().unwrap();
            db::sessions::find_headless_session(&conn)
        })
        .await
    }

    /// List tasks for a session.
    pub async fn task_list(
        &self,
        session_id: &SessionId,
        time_range: Option<TimeRange>,
    ) -> StoreResult<Vec<TaskListItem>> {
        let this = self.clone();
        let sid = session_id.clone();
        let tr = time_range.clone();
        Self::blocking(move || {
            let conn = this.inner.conn.lock().unwrap();
            db::tasks::list_tasks(&conn, &sid, tr.as_ref())
        })
        .await
    }

    /// Rename a session.
    pub async fn session_rename(
        &self,
        session_id: &SessionId,
        new_name: Option<&str>,
    ) -> StoreResult<Session> {
        let this = self.clone();
        let sid = session_id.clone();
        let name = new_name.map(String::from);
        Self::blocking(move || {
            let conn = this.inner.conn.lock().unwrap();
            let updated = db::sessions::rename_session(&conn, &sid, name.as_deref())?;
            if !updated {
                return Err(StoreError::NotFound(format!("session '{}' not found", sid)));
            }
            db::sessions::get_session(&conn, &sid)?
                .ok_or_else(|| StoreError::NotFound("session vanished".into()))
        })
        .await
    }

    /// Delete task detail while preserving the session's historical aggregates.
    pub async fn task_delete(&self, task_id: &TaskId) -> StoreResult<()> {
        let this = self.clone();
        let tid = task_id.clone();
        Self::blocking(move || {
            let conn = this.inner.conn.lock().unwrap();
            if !db::tasks::delete_task(&conn, &tid)? {
                return Err(StoreError::NotFound(format!("task '{}' not found", tid)));
            }
            Ok(())
        })
        .await
    }

    pub async fn task_delete_batch(&self, task_ids: &[TaskId]) -> StoreResult<usize> {
        let this = self.clone();
        let tids = task_ids.to_vec();
        Self::blocking(move || {
            let mut conn = this.inner.conn.lock().unwrap();
            let tx = conn.transaction()?;
            let deleted = db::tasks::delete_tasks(&tx, &tids)?;
            tx.commit()?;
            Ok(deleted)
        })
        .await
    }

    pub async fn session_delete(&self, session_id: &SessionId) -> StoreResult<()> {
        let this = self.clone();
        let sid = session_id.clone();
        Self::blocking(move || {
            let conn = this.inner.conn.lock().unwrap();
            if !this.inner.archive.delete_session(&conn, &sid)? {
                return Err(StoreError::NotFound(format!("session '{}' not found", sid)));
            }
            Ok(())
        })
        .await
    }

    pub async fn session_stop(&self, session_id: &SessionId, ended_at: i64) -> StoreResult<bool> {
        let this = self.clone();
        let sid = session_id.clone();
        Self::blocking(move || {
            let conn = this.inner.conn.lock().unwrap();
            db::sessions::stop_session(&conn, &sid, ended_at)
        })
        .await
    }

    // ── Archive ──

    /// Archive sessions. If session_ids is None, archives all dirty sessions.
    pub async fn archive_create(
        &self,
        session_ids: Option<&[SessionId]>,
        options: ArchiveOptions,
    ) -> StoreResult<Vec<ArchiveInfo>> {
        let this = self.clone();
        let ids: Option<Vec<SessionId>> = session_ids.map(|s| s.to_vec());
        Self::blocking(move || {
            let conn = this.inner.conn.lock().unwrap();
            let ids = match ids {
                Some(v) => v,
                None => {
                    let filter = SessionFilter::default();
                    let sessions = db::sessions::list_sessions(&conn, &filter)?;
                    sessions
                        .into_iter()
                        .filter(|s| options.force || s.archive_dirty)
                        .map(|s| s.id)
                        .collect()
                }
            };
            let mut results = Vec::new();
            for id in &ids {
                results.push(this.inner.archive.archive_session(&conn, id, &options)?);
            }
            Ok(results)
        })
        .await
    }

    /// List archive files on disk.
    pub async fn archive_list(&self, filter: Option<&str>) -> StoreResult<Vec<ArchiveInfo>> {
        let this = self.clone();
        let f = filter.map(String::from);
        Self::blocking(move || this.inner.archive.list_archives(f.as_deref())).await
    }

    /// Read the raw content of an archive YAML file.
    pub async fn archive_read(&self, filename: &str) -> StoreResult<String> {
        let this = self.clone();
        let name = filename.to_string();
        Self::blocking(move || this.inner.archive.read_file(&name)).await
    }

    /// Full-text search across archive YAML files.
    pub async fn archive_search(
        &self,
        query: &str,
        role_filter: Option<&str>,
    ) -> StoreResult<Vec<ArchiveSearchResult>> {
        let this = self.clone();
        let q = query.to_string();
        let rf = role_filter.map(String::from);
        Self::blocking(move || this.inner.archive.search(&q, rf.as_deref())).await
    }

    /// Rename a session in both the DB and the archive YAML file.
    pub async fn archive_rename_session(
        &self,
        session_id: &SessionId,
        new_name: Option<&str>,
    ) -> StoreResult<()> {
        let this = self.clone();
        let sid = session_id.clone();
        let name = new_name.map(String::from);
        Self::blocking(move || {
            let conn = this.inner.conn.lock().unwrap();
            this.inner
                .archive
                .rename_session(&conn, &sid, name.as_deref())
        })
        .await
    }

    /// Get the archive directory path.
    pub fn archive_dir(&self) -> &PathBuf {
        self.inner.archive.archive_dir()
    }

    /// Clean up tasks older than retention for all archived sessions.
    pub async fn cleanup_tasks(&self, retention_hours: u64) -> StoreResult<u64> {
        let this = self.clone();
        Self::blocking(move || {
            let conn = this.inner.conn.lock().unwrap();
            let filter = SessionFilter::default();
            let sessions = db::sessions::list_sessions(&conn, &filter)?;
            let now_ms = chrono::Utc::now().timestamp_millis();
            let cutoff_ms = now_ms - (retention_hours as i64 * 3600 * 1000);
            let mut total = 0u64;
            for s in &sessions {
                if s.last_archived_sequence > 0 && retention_hours > 0 {
                    total += db::tasks::cleanup_old_tasks(
                        &conn,
                        &s.id,
                        s.last_archived_sequence,
                        cutoff_ms,
                    )? as u64;
                }
            }
            Ok(total)
        })
        .await
    }

    /// Delete archived session rows according to age/count retention.
    /// Archive YAML files remain available as the durable record.
    pub async fn cleanup_sessions(
        &self,
        delete_after_days: u64,
        max_sessions: u64,
    ) -> StoreResult<u64> {
        let this = self.clone();
        Self::blocking(move || {
            let mut conn = this.inner.conn.lock().unwrap();
            let sessions = db::sessions::list_sessions(&conn, &SessionFilter::default())?;
            let cutoff = chrono::Utc::now().timestamp_millis()
                - (delete_after_days as i64 * 24 * 3600 * 1000);
            let mut ids = std::collections::HashSet::new();
            for (index, session) in sessions.iter().enumerate() {
                if session.last_archived_sequence == 0 {
                    continue;
                }
                let too_old = delete_after_days > 0 && session.last_activity_at < cutoff;
                let over_limit = max_sessions > 0 && index >= max_sessions as usize;
                if too_old || over_limit {
                    ids.insert(session.id.clone());
                }
            }
            let tx = conn.transaction()?;
            let mut deleted = 0u64;
            for id in ids {
                deleted += u64::from(db::sessions::delete_session(&tx, &id)?);
            }
            tx.commit()?;
            Ok(deleted)
        })
        .await
    }

    // ── Summary ──

    /// Get task summary with full analysis (SessionSummary).
    pub async fn summary_get(&self, task_id: &TaskId) -> StoreResult<SessionSummary> {
        let this = self.clone();
        let tid = task_id.clone();
        Self::blocking(move || {
            let conn = this.inner.conn.lock().unwrap();
            let task = db::tasks::get_task(&conn, &tid)?
                .ok_or_else(|| StoreError::NotFound(format!("task '{}' not found", tid)))?;
            crate::summary::analyzer::analyze_task(&task)
                .ok_or_else(|| StoreError::NotFound(format!("could not analyze task '{}'", tid)))
        })
        .await
    }

    /// Generate and cache a task summary in one atomic operation.
    /// Fire-and-forget: errors are logged but not returned.
    pub fn summary_cache(&self, task_id: &TaskId, session_id: &SessionId) {
        let this = self.clone();
        let tid = task_id.clone();
        let sid = session_id.clone();
        tokio::task::spawn_blocking(move || {
            let conn = this.inner.conn.lock().unwrap();
            let task = match db::tasks::get_task(&conn, &tid) {
                Ok(Some(t)) => t,
                _ => return,
            };
            if let Some(summary) = crate::summary::analyzer::analyze_task(&task) {
                if let Ok(json) = serde_json::to_string(&summary) {
                    let _ = db::tasks::update_summary(&conn, &tid, &json);
                    let _ = db::sessions::set_archive_dirty(&conn, &sid);
                }
            }
        });
    }

    /// Query daily usage for a date range.
    pub async fn usage_query_range(
        &self,
        from: &str,
        to: &str,
    ) -> StoreResult<Vec<crate::db::usage::DailyUsageRow>> {
        let this = self.clone();
        let f = from.to_string();
        let t = to.to_string();
        Self::blocking(move || {
            let conn = this.inner.conn.lock().unwrap();
            crate::db::usage::query_range(&conn, &f, &t)
        })
        .await
    }

    /// Get aggregated cost stats for today and current month.
    pub async fn usage_cost_stats(&self) -> StoreResult<proxy_common::models::CostStats> {
        let this = self.clone();
        Self::blocking(move || {
            let conn = this.inner.conn.lock().unwrap();
            crate::db::usage::query_cost_stats(&conn)
        })
        .await
    }

    // ── Batch commands ──

    /// Run a batch command (archive or summary generation).
    pub async fn run(&self, command: RunCommand) -> StoreResult<RunResult> {
        match command {
            RunCommand::Archive {
                session_ids,
                options,
            } => {
                let results = self.archive_create(session_ids.as_deref(), options).await?;
                Ok(RunResult::Archive(results))
            }
            RunCommand::Summary { task_ids } => {
                let this = self.clone();
                Self::blocking(move || {
                    let conn = this.inner.conn.lock().unwrap();
                    let mut processed = 0usize;

                    match task_ids {
                        Some(ids) => {
                            for id in &ids {
                                let task = match db::tasks::get_task(&conn, id)? {
                                    Some(t) => t,
                                    None => continue,
                                };
                                if task.summary_json.is_none() {
                                    if let Some(summary) =
                                        crate::summary::analyzer::analyze_task(&task)
                                    {
                                        let json = serde_json::to_string(&summary)?;
                                        db::tasks::update_summary(&conn, id, &json)?;
                                        let _ = db::sessions::set_archive_dirty(
                                            &conn,
                                            &task.session_id,
                                        );
                                        processed += 1;
                                    }
                                }
                            }
                        }
                        None => {
                            let filter = crate::models::SessionFilter::default();
                            let sessions = db::sessions::list_sessions(&conn, &filter)?;
                            for s in &sessions {
                                let tasks = db::tasks::list_tasks(&conn, &s.id, None)?;
                                for t in &tasks {
                                    if t.summary_json.is_none() {
                                        let full_task = match db::tasks::get_task(&conn, &t.id)? {
                                            Some(ft) => ft,
                                            None => continue,
                                        };
                                        if let Some(summary) =
                                            crate::summary::analyzer::analyze_task(&full_task)
                                        {
                                            let json = serde_json::to_string(&summary)?;
                                            db::tasks::update_summary(&conn, &t.id, &json)?;
                                            let _ = db::sessions::set_archive_dirty(&conn, &s.id);
                                            processed += 1;
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Ok(RunResult::Summary { processed })
                })
                .await
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::NewSessionDefaults;
    use proxy_common::{BillingSnapshot, ClientType, PriceRates, TaskStatus, TaskUsage};

    fn temp_store() -> (ProxyStore, PathBuf) {
        let root = std::env::temp_dir().join(format!("cc-proxy-store-{}", ulid::Ulid::new()));
        let store = ProxyStore::open(ProxyStoreConfig {
            database_path: root.join("proxy.db"),
            archive_dir: root.join("archives"),
            ..Default::default()
        })
        .unwrap();
        (store, root)
    }

    fn task(id: &str, started_at: i64) -> NewTask {
        NewTask {
            id: Some(TaskId::new(id.into())),
            session_defaults: NewSessionDefaults {
                client_type: ClientType::ClaudeCode,
                client_session_id: Some("session-test".into()),
                ..Default::default()
            },
            started_at,
            first_byte_at: Some(started_at + 10),
            ended_at: Some(started_at + 50),
            status: TaskStatus::Completed,
            method: "POST".into(),
            path: "/v1/messages".into(),
            request_headers: None,
            request_body: Some(r#"{"messages":[{"role":"user","content":"hello"}]}"#.into()),
            response_headers: None,
            response_body: Some(proxy_common::NormalizedResponse {
                text: vec!["world".into()],
                ..Default::default()
            }),
            http_status_code: Some(200),
            is_streaming: false,
            requested_model: Some("model".into()),
            upstream: Some("upstream".into()),
            billing: BillingSnapshot {
                pricing_model_id: "priced".into(),
                provider: "provider".into(),
                resolved_model: "model".into(),
                rates: PriceRates {
                    input_microusd: 1_000_000,
                    output_microusd: 1_000_000,
                    ..Default::default()
                },
                currency: "USD".into(),
            },
            usage: TaskUsage {
                input_tokens: 10,
                output_tokens: 20,
                ..Default::default()
            },
            timing: crate::models::TaskTiming {
                duration_ms: Some(50),
                ttft_ms: Some(10),
                stop_reason: Some("end_turn".into()),
                ..Default::default()
            },
            error: None,
            metadata: serde_json::json!({}),
            messages_count: 1,
            summary_json: None,
        }
    }

    #[tokio::test]
    async fn write_rolls_back_all_tables_when_daily_usage_fails() {
        let (store, root) = temp_store();
        {
            let conn = store.inner.conn.lock().unwrap();
            conn.execute_batch(
                "CREATE TRIGGER fail_daily BEFORE INSERT ON session_daily_usage
                 BEGIN SELECT RAISE(FAIL, 'injected daily failure'); END;",
            )
            .unwrap();
        }
        let sid = SessionId::new("session-test".into()).unwrap();
        assert!(store
            .task_write(&sid, task("task-rollback", 1_700_000_000_000))
            .await
            .is_err());
        let conn = store.inner.conn.lock().unwrap();
        let tasks: i64 = conn
            .query_row("SELECT COUNT(*) FROM tasks", [], |r| r.get(0))
            .unwrap();
        let sessions: i64 = conn
            .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
            .unwrap();
        let usage: i64 = conn
            .query_row("SELECT COUNT(*) FROM session_daily_usage", [], |r| r.get(0))
            .unwrap();
        assert_eq!((tasks, sessions, usage), (0, 0, 0));
        drop(conn);
        drop(store);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn deleting_task_detail_preserves_session_authority() {
        let (store, root) = temp_store();
        let sid = SessionId::new("session-test".into()).unwrap();
        let written = store
            .task_write(&sid, task("task-preserve", 1_700_000_000_000))
            .await
            .unwrap();
        store.task_delete(&written.id).await.unwrap();
        assert!(store.task_info(&written.id).await.is_err());
        let session = store.session_get(&sid).await.unwrap().unwrap();
        assert_eq!(session.task_count, 1);
        assert_eq!(session.total_input_tokens, 10);
        assert_eq!(session.total_output_tokens, 20);
        assert_eq!(session.last_task_id.as_ref(), Some(&written.id));
        assert_eq!(session.total_ttft_ms, 10);
        drop(store);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn deleting_session_also_removes_its_archive() {
        let (store, root) = temp_store();
        let sid = SessionId::new("session-test".into()).unwrap();
        store
            .task_write(&sid, task("task-archive-delete", 1_700_000_000_000))
            .await
            .unwrap();
        store
            .archive_create(
                Some(std::slice::from_ref(&sid)),
                ArchiveOptions {
                    task_retention_hours: 0,
                    force: true,
                },
            )
            .await
            .unwrap();
        let archive_path = root.join("archives").join("session-test.yaml");
        assert!(archive_path.exists());
        store.session_delete(&sid).await.unwrap();
        assert!(!archive_path.exists());
        assert!(store.session_get(&sid).await.unwrap().is_none());
        drop(store);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn persisted_summary_contains_all_tasks_without_raw_messages() {
        let (store, root) = temp_store();
        let sid = SessionId::new("session-test".into()).unwrap();
        store
            .task_write(&sid, task("task-summary-1", 1_700_000_000_000))
            .await
            .unwrap();
        store
            .task_write(&sid, task("task-summary-2", 1_700_000_001_000))
            .await
            .unwrap();

        store
            .archive_create(
                Some(std::slice::from_ref(&sid)),
                ArchiveOptions {
                    task_retention_hours: 1,
                    force: true,
                },
            )
            .await
            .unwrap();
        assert!(store.task_list(&sid, None).await.unwrap().is_empty());
        store
            .archive_create(
                Some(std::slice::from_ref(&sid)),
                ArchiveOptions {
                    task_retention_hours: 0,
                    force: true,
                },
            )
            .await
            .unwrap();

        let yaml = std::fs::read_to_string(root.join("archives/session-test.yaml")).unwrap();
        let document: crate::archive::format::ArchiveDocument =
            serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(document.version, 3);
        assert_eq!(document.statistics.task_count, 2);
        assert_eq!(document.tasks.len(), 2);
        assert!(document.tasks.iter().all(|task| task.summary.is_some()));
        assert!(!yaml.contains("request_body:"));
        assert!(!yaml.contains("response_body:"));
        assert!(!yaml.contains("\n    messages:"));
        drop(store);
        let _ = std::fs::remove_dir_all(root);
    }
}
