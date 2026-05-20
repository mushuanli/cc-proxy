use proxy_common::SessionId;
use rusqlite::Connection;
use std::path::PathBuf;

use crate::archive::file;
use crate::archive::format::build_archive;
use crate::db::{sessions, tasks};
use crate::error::StoreResult;
use crate::models::{ArchiveInfo, ArchiveOptions};

/// Archive manager: writes session YAML files and cleans up old tasks.
pub struct ArchiveManager {
    pub archive_dir: PathBuf,
}

impl ArchiveManager {
    pub fn new(archive_dir: PathBuf) -> Self {
        Self { archive_dir }
    }

    /// Ensure the archive directory exists.
    pub fn init(&self) -> StoreResult<()> {
        std::fs::create_dir_all(&self.archive_dir)?;
        file::cleanup_tmp_files(&self.archive_dir)?;
        Ok(())
    }

    /// Archive a single session.
    pub fn archive_session(
        &self,
        conn: &Connection,
        session_id: &SessionId,
        options: &ArchiveOptions,
    ) -> StoreResult<ArchiveInfo> {
        let session = sessions::get_session(conn, session_id)?
            .ok_or_else(|| crate::error::StoreError::NotFound(format!(
                "session '{}' not found",
                session_id
            )))?;

        if !options.force && !session.archive_dirty {
            return Ok(ArchiveInfo {
                session_id: session.id.clone(),
                name: session.name.clone(),
                file_path: self.session_archive_path(session_id),
                archived_at: session.last_archived_at,
                task_count: session.task_count,
            });
        }

        let latest_task = tasks::get_latest_completed_task(conn, session_id)?;
        let daily_usage = crate::db::usage::get_session_daily_usage(conn, session_id)?;

        let doc = build_archive(&session, latest_task.as_ref(), &daily_usage);
        let yaml = serde_yaml::to_string(&doc)?;

        let file_path = self.session_archive_path(session_id);
        file::atomic_write(
            &std::path::PathBuf::from(&file_path),
            &yaml,
        )?;

        let now_ms = chrono::Utc::now().timestamp_millis();
        let checkpoint_task_id = latest_task
            .as_ref()
            .map(|t| t.id.as_str().to_string())
            .unwrap_or_default();
        let checkpoint_seq = latest_task
            .as_ref()
            .map(|t| t.sequence_no)
            .unwrap_or(0);

        sessions::update_archive_checkpoint(
            conn,
            session_id,
            now_ms,
            &checkpoint_task_id,
            checkpoint_seq,
        )?;

        // Clean up old tasks past retention
        if options.task_retention_hours > 0 {
            let cutoff_ms = now_ms - (options.task_retention_hours as i64 * 3600 * 1000);
            tasks::cleanup_old_tasks(
                conn,
                session_id,
                checkpoint_seq,
                cutoff_ms,
            )?;
        }

        Ok(ArchiveInfo {
            session_id: session.id.clone(),
            name: session.name.clone(),
            file_path,
            archived_at: Some(now_ms),
            task_count: session.task_count,
        })
    }

    /// List all archive files on disk.
    pub fn list_archives(&self, filter: Option<&str>) -> StoreResult<Vec<ArchiveInfo>> {
        let mut result = Vec::new();
        if !self.archive_dir.is_dir() {
            return Ok(result);
        }

        for entry in std::fs::read_dir(&self.archive_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("yaml") {
                continue;
            }

            let file_name = path
                .file_stem()
                .and_then(|n| n.to_str())
                .unwrap_or("");

            if let Some(ref f) = filter {
                if !file_name.contains(f) {
                    continue;
                }
            }

            let session_id = SessionId::new(file_name.to_string());
            result.push(ArchiveInfo {
                session_id,
                name: None,
                file_path: path.to_string_lossy().into_owned(),
                archived_at: None,
                task_count: 0,
            });
        }

        Ok(result)
    }

    fn session_archive_path(&self, session_id: &SessionId) -> String {
        self.archive_dir
            .join(format!("{}.yaml", session_id.as_str()))
            .to_string_lossy()
            .into_owned()
    }
}
