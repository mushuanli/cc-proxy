use proxy_common::{SessionId, TaskId};
use rusqlite::Connection;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::archive::manager::ArchiveManager;
use crate::command::{RunCommand, RunResult};
use crate::db::{self, connection, migration};
use crate::error::StoreResult;
use crate::models::{
    ArchiveInfo, ArchiveOptions, NewTask, Session, SessionFilter, SessionListItem, Task,
    TaskListItem, TimeRange,
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
        let sid_short = session_id.as_str().len();
        let sid_short = if sid_short > 8 {
            &session_id.as_str()[sid_short - 8..]
        } else {
            session_id.as_str()
        };
        let task_id_debug = task
            .id
            .as_ref()
            .map(|id| {
                let s = id.as_str();
                if s.len() > 8 { &s[s.len() - 8..] } else { s }
            })
            .unwrap_or("new");

        let op = || -> StoreResult<Task> {
            let conn = self.inner.conn.lock().unwrap();

            // Compute cost from billing snapshot + usage (store owns billing logic)
            let cost_microusd = crate::billing::calculate_cost_microusd(
                &task.usage,
                &task.billing.rates,
            )
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
            let inserted = db::tasks::insert_task(&conn, &task, &task_id, session_id, sequence_no, cost_microusd)?;

            if inserted {
                // Update session aggregates
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

            db::tasks::get_task(&conn, &task_id)?
                .ok_or_else(|| crate::error::StoreError::NotFound("task not found after write".into()))
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
        db::tasks::get_task(&conn, task_id)?
            .ok_or_else(|| crate::error::StoreError::NotFound(format!("task '{}' not found", task_id)))
    }

    /// List sessions with optional filters.
    pub fn list_sessions(&self, filter: SessionFilter) -> StoreResult<Vec<SessionListItem>> {
        let conn = self.inner.conn.lock().unwrap();
        db::sessions::list_sessions(&conn, &filter)
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
    pub fn name(
        &self,
        session_id: &SessionId,
        new_name: Option<&str>,
    ) -> StoreResult<Session> {
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

    // ── Summary ──

    /// Get task summary with full analysis (SessionSummary).
    /// Analyzes the task's request body and returns structured conversation data.
    pub fn summary(&self, task_id: &TaskId) -> StoreResult<SessionSummary> {
        let conn = self.inner.conn.lock().unwrap();

        let task = db::tasks::get_task(&conn, task_id)?
            .ok_or_else(|| crate::error::StoreError::NotFound(format!(
                "task '{}' not found",
                task_id
            )))?;

        // Build a minimal ProxiedRequest for the analyzer
        let req = proxy_common::models::ProxiedRequest {
            request_body: task.request_body.clone(),
            model: task.requested_model.clone(),
            ..Default::default()
        };

        crate::summary::analyzer::analyze_request(&req)
            .ok_or_else(|| crate::error::StoreError::NotFound(format!(
                "could not analyze task '{}'",
                task_id
            )))
    }

    /// Query daily usage for a date range. Returns raw rows from session_daily_usage.
    pub fn query_daily_usage_range(&self, from: &str, to: &str) -> StoreResult<Vec<crate::db::usage::DailyUsageRow>> {
        let conn = self.inner.conn.lock().unwrap();
        crate::db::usage::query_range(&conn, from, to)
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
