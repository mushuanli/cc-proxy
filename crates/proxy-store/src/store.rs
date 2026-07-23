use proxy_common::{SessionId, TaskId};
use rusqlite::Connection;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::archive::manager::ArchiveManager;
use crate::command::{RunCommand, RunResult};
use crate::db::{self, connection, migration};
use crate::error::StoreResult;
use crate::models::{
    ArchiveInfo, ArchiveOptions, ArchiveSearchResult, NewTask, Session, SessionFilter,
    SessionListItem, Task, TaskListItem, TimeRange,
};
use crate::summary::analyzer::SessionSummary;

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
    pub fn open(config: ProxyStoreConfig) -> StoreResult<Self> {
        // Create directories
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

    // ── Write ──

    /// Write a new task. Auto-creates the session if it doesn't exist.
    /// Idempotent: if a TaskId is provided and already exists, no aggregates are updated.
    ///
    /// Cost is computed internally from `task.billing.rates` and `task.usage` —
    /// the caller does not need to calculate cost.
    pub fn write(&self, session_id: &SessionId, task: NewTask) -> StoreResult<Task> {
        // SessionId is validated ASCII-only, safe for byte slicing
        let sid_str = session_id.as_str();
        let sid_short = if sid_str.len() > 8 {
            &sid_str[sid_str.len() - 8..]
        } else {
            sid_str
        };
        let task_id_debug = task
            .id
            .as_ref()
            .map(|id| {
                let s = id.as_str();
                if s.len() > 8 {
                    &s[s.len() - 8..]
                } else {
                    s
                }
            })
            .unwrap_or("new");

        let op = || -> StoreResult<Task> {
            let conn = self.inner.conn.lock().unwrap();

            // Wrap all writes in a transaction for atomicity
            conn.execute_batch("BEGIN IMMEDIATE")?;

            let result = (|| -> StoreResult<Task> {
                // Compute cost from billing snapshot + usage (store owns billing logic)
                let cost_microusd =
                    crate::billing::calculate_cost_microusd(&task.usage, &task.billing.rates)
                        .map_err(|e| crate::error::StoreError::InvalidArgument(e.to_string()))?;

                // Generate or use provided task id
                let task_id = task.id.clone().unwrap_or_else(TaskId::generate);

                // Check if task already exists (idempotency)
                if db::tasks::get_task(&conn, &task_id)?.is_some() {
                    return db::tasks::get_task(&conn, &task_id)?
                        .ok_or_else(|| crate::error::StoreError::NotFound("task vanished".into()));
                }

                let now_ms = chrono::Utc::now().timestamp_millis();

                // Ensure session exists
                db::sessions::ensure_session(&conn, session_id, &task.session_defaults, now_ms)?;

                // Allocate sequence number
                let sequence_no = db::sessions::allocate_sequence(&conn, session_id)?;

                // Insert task
                let inserted = db::tasks::insert_task(
                    &conn,
                    &task,
                    &task_id,
                    session_id,
                    sequence_no,
                    cost_microusd,
                )?;

                if inserted {
                    // Update session aggregates
                    let priced = task.billing.pricing_model_id != "unknown";
                    let duration_ms = task.timing.duration_ms.unwrap_or(0);
                    db::sessions::update_aggregates(
                        &conn,
                        session_id,
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
                        task.ended_at,
                    )?;

                    // Upsert daily usage
                    db::usage::upsert_daily_usage(
                        &conn,
                        session_id,
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

                db::tasks::get_task(&conn, &task_id)?.ok_or_else(|| {
                    crate::error::StoreError::NotFound("task not found after write".into())
                })
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
        };

        op().map_err(|e| {
            tracing::error!(
                "[store] write failed: sid=…{} task={} error={}",
                sid_short,
                task_id_debug,
                e
            );
            e
        })
    }

    // ── Read ──

    /// Get full task details by id.
    pub fn info(&self, task_id: &TaskId) -> StoreResult<Task> {
        let conn = self.inner.conn.lock().unwrap();
        db::tasks::get_task(&conn, task_id)?.ok_or_else(|| {
            crate::error::StoreError::NotFound(format!("task '{}' not found", task_id))
        })
    }

    /// List sessions with optional filters.
    pub fn list_sessions(&self, filter: SessionFilter) -> StoreResult<Vec<SessionListItem>> {
        let conn = self.inner.conn.lock().unwrap();
        db::sessions::list_sessions(&conn, &filter)
    }

    /// Get a single session by exact ID.
    pub fn get_session(&self, id: &SessionId) -> StoreResult<Option<Session>> {
        let conn = self.inner.conn.lock().unwrap();
        db::sessions::get_session(&conn, id)
    }

    /// List tasks for a session.
    pub fn list_tasks(
        &self,
        session_id: &SessionId,
        time_range: Option<TimeRange>,
    ) -> StoreResult<Vec<TaskListItem>> {
        let conn = self.inner.conn.lock().unwrap();
        db::tasks::list_tasks(&conn, session_id, time_range.as_ref())
    }

    /// Rename a session.
    pub fn name(&self, session_id: &SessionId, new_name: Option<&str>) -> StoreResult<Session> {
        let conn = self.inner.conn.lock().unwrap();
        let updated = db::sessions::rename_session(&conn, session_id, new_name)?;
        if !updated {
            return Err(crate::error::StoreError::NotFound(format!(
                "session '{}' not found",
                session_id
            )));
        }
        db::sessions::get_session(&conn, session_id)?
            .ok_or_else(|| crate::error::StoreError::NotFound("session vanished".into()))
    }

    // ── Archive ──

    /// Archive sessions. If session_ids is None, archives all dirty sessions.
    pub fn archive(
        &self,
        session_ids: Option<&[SessionId]>,
        options: ArchiveOptions,
    ) -> StoreResult<Vec<ArchiveInfo>> {
        let conn = self.inner.conn.lock().unwrap();

        let ids: Vec<SessionId> = match session_ids {
            Some(ids) => ids.to_vec(),
            None => {
                // Get all sessions with archive_dirty = 1
                let filter = SessionFilter::default();
                let sessions = db::sessions::list_sessions(&conn, &filter)?;
                sessions
                    .into_iter()
                    .filter(|s| s.archive_dirty)
                    .map(|s| s.id)
                    .collect()
            }
        };

        let mut results = Vec::new();
        for id in &ids {
            let info = self.inner.archive.archive_session(&conn, id, &options)?;
            results.push(info);
        }

        Ok(results)
    }

    /// List archive files on disk.
    pub fn list_archives(&self, filter: Option<&str>) -> StoreResult<Vec<ArchiveInfo>> {
        self.inner.archive.list_archives(filter)
    }

    /// Read the raw content of an archive YAML file.
    pub fn read_archive_file(&self, filename: &str) -> StoreResult<String> {
        self.inner.archive.read_file(filename)
    }

    /// Full-text search across archive YAML files.
    pub fn search_archives(
        &self,
        query: &str,
        role_filter: Option<&str>,
    ) -> StoreResult<Vec<ArchiveSearchResult>> {
        self.inner.archive.search(query, role_filter)
    }

    /// Rename a session in both the DB and the archive YAML file.
    pub fn rename_archive_session(
        &self,
        session_id: &SessionId,
        new_name: Option<&str>,
    ) -> StoreResult<()> {
        let conn = self.inner.conn.lock().unwrap();
        self.inner
            .archive
            .rename_session(&conn, session_id, new_name)
    }

    /// Get the archive directory path.
    pub fn archive_dir(&self) -> &PathBuf {
        self.inner.archive.archive_dir()
    }

    // ── Summary ──

    /// Get task summary with full analysis (SessionSummary).
    /// Analyzes the task's request body and returns structured conversation data.
    pub fn summary(&self, task_id: &TaskId) -> StoreResult<SessionSummary> {
        let conn = self.inner.conn.lock().unwrap();

        let task = db::tasks::get_task(&conn, task_id)?.ok_or_else(|| {
            crate::error::StoreError::NotFound(format!("task '{}' not found", task_id))
        })?;

        crate::summary::analyzer::analyze_task(&task).ok_or_else(|| {
            crate::error::StoreError::NotFound(format!("could not analyze task '{}'", task_id))
        })
    }

    /// Query daily usage for a date range. Returns raw rows from session_daily_usage.
    pub fn query_daily_usage_range(
        &self,
        from: &str,
        to: &str,
    ) -> StoreResult<Vec<crate::db::usage::DailyUsageRow>> {
        let conn = self.inner.conn.lock().unwrap();
        crate::db::usage::query_range(&conn, from, to)
    }

    /// Get aggregated cost stats for today and current month.
    /// Used by relay to push real-time cost updates via WebSocket.
    pub fn get_cost_stats(&self) -> StoreResult<proxy_common::models::CostStats> {
        let conn = self.inner.conn.lock().unwrap();
        crate::db::usage::query_cost_stats(&conn)
    }

    // ── Batch commands ──

    /// Run a batch command (archive or summary generation).
    pub fn run(&self, command: RunCommand) -> StoreResult<RunResult> {
        match command {
            RunCommand::Archive {
                session_ids,
                options,
            } => {
                let results = self.archive(session_ids.as_deref(), options)?;
                Ok(RunResult::Archive(results))
            }
            RunCommand::Summary { task_ids } => {
                let conn = self.inner.conn.lock().unwrap();
                let mut processed = 0usize;

                match task_ids {
                    Some(ids) => {
                        for id in &ids {
                            if crate::summary::cache::get_summary(&conn, id)?.is_some() {
                                processed += 1;
                            }
                        }
                    }
                    None => {
                        // Process all tasks without summary, status != recording
                        // This is a simplified approach; a full implementation would
                        // query for tasks needing summary generation
                        processed = 0;
                    }
                }

                Ok(RunResult::Summary { processed })
            }
        }
    }
}
