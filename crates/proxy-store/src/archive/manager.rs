use proxy_common::SessionId;
use rusqlite::Connection;
use std::path::PathBuf;

use crate::archive::file;
use crate::archive::format::build_archive;
use crate::db::{sessions, tasks};
use crate::error::StoreResult;
use crate::models::{ArchiveInfo, ArchiveOptions, ArchiveSearchResult, ArchiveSnippet};

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
        let session = sessions::get_session(conn, session_id)?.ok_or_else(|| {
            crate::error::StoreError::NotFound(format!("session '{}' not found", session_id))
        })?;

        if !options.force && !session.archive_dirty {
            return self.archive_info(&session, session.last_archived_at);
        }

        let (file_path, latest_task) = self.persist_summary(conn, &session)?;
        let now_ms = Self::checkpoint_and_cleanup(conn, session_id, latest_task.as_ref(), options)?;
        Ok(ArchiveInfo {
            session_id: session.id,
            name: session.name,
            file_path,
            archived_at: Some(now_ms),
            task_count: session.task_count,
        })
    }

    fn persist_summary(
        &self,
        conn: &Connection,
        session: &crate::models::Session,
    ) -> StoreResult<(String, Option<crate::models::Task>)> {
        let session_id = &session.id;
        let summary_tasks = Self::load_summary_tasks(conn, session_id)?;
        let latest_task = tasks::get_latest_completed_task(conn, session_id)?;
        let daily_usage = crate::db::usage::get_session_daily_usage(conn, session_id)?;
        let file_path = self.session_archive_path(session_id)?;
        let mut doc = build_archive(&session, &summary_tasks, &daily_usage);
        Self::merge_existing_tasks(&file_path, &mut doc);
        let yaml = serde_yaml::to_string(&doc)?;
        file::atomic_write(&std::path::PathBuf::from(&file_path), &yaml)?;
        Ok((file_path, latest_task))
    }

    fn checkpoint_and_cleanup(
        conn: &Connection,
        session_id: &SessionId,
        latest_task: Option<&crate::models::Task>,
        options: &ArchiveOptions,
    ) -> StoreResult<i64> {
        let now_ms = chrono::Utc::now().timestamp_millis();
        let checkpoint_task_id = latest_task
            .map(|t| t.id.as_str().to_string())
            .unwrap_or_default();
        let checkpoint_seq = latest_task.map(|t| t.sequence_no).unwrap_or(0);
        sessions::update_archive_checkpoint(
            conn,
            session_id,
            now_ms,
            &checkpoint_task_id,
            checkpoint_seq,
        )?;
        if options.task_retention_hours > 0 {
            let cutoff_ms = now_ms - (options.task_retention_hours as i64 * 3600 * 1000);
            tasks::cleanup_old_tasks(conn, session_id, checkpoint_seq, cutoff_ms)?;
        }
        Ok(now_ms)
    }

    fn archive_info(
        &self,
        session: &crate::models::Session,
        archived_at: Option<i64>,
    ) -> StoreResult<ArchiveInfo> {
        Ok(ArchiveInfo {
            session_id: session.id.clone(),
            name: session.name.clone(),
            file_path: self.session_archive_path(&session.id)?,
            archived_at,
            task_count: session.task_count,
        })
    }

    fn load_summary_tasks(
        conn: &Connection,
        session_id: &SessionId,
    ) -> StoreResult<Vec<crate::models::Task>> {
        let items = tasks::list_tasks(conn, session_id, None)?;
        let mut result = Vec::with_capacity(items.len());
        for item in items {
            if let Some(mut task) = tasks::get_task(conn, &item.id)? {
                Self::ensure_task_summary(conn, &mut task)?;
                result.push(task);
            }
        }
        Ok(result)
    }

    fn ensure_task_summary(conn: &Connection, task: &mut crate::models::Task) -> StoreResult<()> {
        if task.summary_json.is_some() {
            return Ok(());
        }
        let Some(summary) = crate::summary::analyzer::analyze_task(task) else {
            return Ok(());
        };
        let json = serde_json::to_string(&summary)?;
        tasks::update_summary(conn, &task.id, &json)?;
        task.summary_json = Some(json);
        Ok(())
    }

    fn merge_existing_tasks(
        file_path: &str,
        document: &mut crate::archive::format::ArchiveDocument,
    ) {
        let Ok(yaml) = std::fs::read_to_string(file_path) else {
            return;
        };
        let Ok(existing) = serde_yaml::from_str::<crate::archive::format::ArchiveDocument>(&yaml)
        else {
            return;
        };
        let current_ids = document
            .tasks
            .iter()
            .map(|task| task.id.clone())
            .collect::<std::collections::HashSet<_>>();
        document.tasks.extend(
            existing
                .tasks
                .into_iter()
                .filter(|task| !current_ids.contains(&task.id)),
        );
        document
            .tasks
            .sort_by(|left, right| right.sequence_no.cmp(&left.sequence_no));
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

            let file_name = path.file_stem().and_then(|n| n.to_str()).unwrap_or("");

            if let Some(ref f) = filter {
                if !file_name.contains(f) {
                    continue;
                }
            }

            let session_id = SessionId::from_trusted(file_name.to_string());
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

    /// Read the raw content of an archive YAML file by filename (e.g. "01JZA.yaml").
    pub fn read_file(&self, filename: &str) -> StoreResult<String> {
        // Reject filenames that try to escape the archive directory
        let safe = filename
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.');
        if !safe || filename.contains("..") {
            return Err(crate::error::StoreError::InvalidArgument(
                "invalid filename".into(),
            ));
        }
        let path = self.archive_dir.join(filename);
        Ok(std::fs::read_to_string(&path)?)
    }

    /// Full-text search across archive YAML files.
    pub fn search(
        &self,
        query: &str,
        role_filter: Option<&str>,
    ) -> StoreResult<Vec<ArchiveSearchResult>> {
        const MAX_FILES: usize = 2_000;
        const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;
        const MAX_TOTAL_BYTES: u64 = 64 * 1024 * 1024;
        const MAX_RESULTS: usize = 200;
        let mut results = Vec::new();
        if !self.archive_dir.is_dir() {
            return Ok(results);
        }

        let q_lower = query.to_lowercase();
        if q_lower.chars().count() > 512 {
            return Err(crate::error::StoreError::InvalidArgument(
                "archive query is too long".into(),
            ));
        }
        if role_filter.is_some_and(|role| !matches!(role, "user" | "assistant" | "system")) {
            return Err(crate::error::StoreError::InvalidArgument(
                "invalid role filter".into(),
            ));
        }
        let mut scanned_files = 0usize;
        let mut scanned_bytes = 0u64;

        for entry in std::fs::read_dir(&self.archive_dir)? {
            if scanned_files >= MAX_FILES
                || scanned_bytes >= MAX_TOTAL_BYTES
                || results.len() >= MAX_RESULTS
            {
                break;
            }
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("yaml") {
                continue;
            }
            let size = path.metadata().map(|m| m.len()).unwrap_or(0);
            if size > MAX_FILE_BYTES || scanned_bytes.saturating_add(size) > MAX_TOTAL_BYTES {
                continue;
            }
            scanned_files += 1;
            scanned_bytes += size;

            let content = match std::fs::read_to_string(&path) {
                Ok(c) => c,
                Err(_) => continue,
            };

            let file = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string();
            // Extract name and last_activity_at from YAML
            let mut name: Option<String> = None;
            let mut last_active_at: Option<String> = None;
            for line in content.lines().take(60) {
                if name.is_none() {
                    if let Some(rest) = line.strip_prefix("  name: ") {
                        let v = rest.trim().trim_matches('"').trim_matches('\'');
                        if !v.is_empty() {
                            name = Some(v.to_string());
                        }
                    }
                }
                if let Some(rest) = line.strip_prefix("  last_activity_at: ") {
                    if let Ok(ts) = rest.trim().parse::<i64>() {
                        if let Some(dt) = chrono::DateTime::from_timestamp(ts / 1000, 0) {
                            last_active_at = Some(dt.to_rfc3339());
                        }
                    }
                }
                if name.is_some() && last_active_at.is_some() {
                    break;
                }
            }

            // Search lines for query matches and extract snippets
            let lines: Vec<&str> = content.lines().collect();
            let mut snippets = Vec::new();
            let mut total_matches = 0usize;

            for (i, line) in lines.iter().enumerate() {
                if !line.to_lowercase().contains(&q_lower) {
                    continue;
                }
                total_matches += 1;
                if snippets.len() >= 10 {
                    continue; // cap snippets per file
                }

                // Determine role by scanning backwards for nearest "role:" line
                let role = (0..=i)
                    .rev()
                    .find_map(|j| lines[j].trim().strip_prefix("role: "))
                    .map(|r| r.trim().trim_matches('"').trim_matches('\'').to_string());

                // Apply role filter
                if let Some(rf) = role_filter {
                    if role.as_deref() != Some(rf) {
                        continue;
                    }
                }

                // Extract context: 2 lines before and after
                let start = i.saturating_sub(2);
                let end = (i + 3).min(lines.len());
                let context: Vec<&str> = lines[start..end]
                    .iter()
                    .map(|l| l.trim())
                    .filter(|l| !l.is_empty())
                    .take(5)
                    .collect();
                let text = context.join("\n");

                snippets.push(ArchiveSnippet {
                    text: text.chars().take(500).collect(),
                    role,
                });
            }

            if total_matches > 0 {
                results.push(ArchiveSearchResult {
                    file,
                    name,
                    last_active_at,
                    size,
                    snippets,
                    match_count: total_matches,
                    role_filter: role_filter.map(String::from),
                    keywords: vec![query.to_string()],
                });
            }
        }

        Ok(results)
    }

    /// Rename a session in its archive YAML file and the database.
    pub fn rename_session(
        &self,
        conn: &Connection,
        session_id: &SessionId,
        new_name: Option<&str>,
    ) -> StoreResult<()> {
        // Defense-in-depth: validate session id before file system operations
        if !file::is_safe_filename(session_id) {
            return Err(crate::error::StoreError::InvalidArgument(format!(
                "unsafe session id for file operation: {}",
                session_id
            )));
        }
        // Update in database if the session still exists there
        let db_updated = sessions::rename_session(conn, session_id, new_name).unwrap_or(false);

        // Update the YAML archive file
        let path = self
            .archive_dir
            .join(format!("{}.yaml", session_id.as_str()));
        if path.exists() {
            let content = std::fs::read_to_string(&path)?;
            let new_content = if let Some(name) = new_name {
                // Replace the first "  name: ..." line under session:
                let mut found_session = false;
                let mut replaced = false;
                let lines: Vec<String> = content
                    .lines()
                    .map(|line| {
                        if !found_session {
                            if line.trim() == "session:" {
                                found_session = true;
                            }
                            line.to_string()
                        } else if !replaced && line.starts_with("  name:") {
                            replaced = true;
                            format!("  name: \"{}\"", name)
                        } else if line.starts_with("statistics:") || line.starts_with("  id:") {
                            // Continue within session block
                            line.to_string()
                        } else {
                            line.to_string()
                        }
                    })
                    .collect();
                lines.join("\n")
            } else {
                // Remove name line
                let mut found_session = false;
                let lines: Vec<&str> = content
                    .lines()
                    .filter(|line| {
                        if !found_session {
                            if line.trim() == "session:" {
                                found_session = true;
                            }
                            true
                        } else {
                            !line.starts_with("  name:")
                        }
                    })
                    .collect();
                lines.join("\n")
            };
            file::atomic_write(&path, &new_content)?;
        } else if !db_updated && new_name.is_some() {
            // Neither DB nor archive file exists — can't rename
            return Err(crate::error::StoreError::NotFound(format!(
                "session '{}' not found in DB or archive",
                session_id
            )));
        }

        Ok(())
    }

    /// Get the archive directory path.
    pub fn archive_dir(&self) -> &PathBuf {
        &self.archive_dir
    }

    /// Delete a session and its archive as one recoverable logical operation.
    pub fn delete_session(&self, conn: &Connection, session_id: &SessionId) -> StoreResult<bool> {
        let archive_path = std::path::PathBuf::from(self.session_archive_path(session_id)?);
        let tombstone = archive_path.with_file_name(format!(
            ".{}.{}.tmp",
            archive_path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("archive"),
            ulid::Ulid::new()
        ));
        let moved_archive = if archive_path.exists() {
            std::fs::rename(&archive_path, &tombstone)?;
            true
        } else {
            false
        };

        conn.execute_batch("BEGIN IMMEDIATE")?;
        match sessions::delete_session(conn, session_id) {
            Ok(true) => {
                if let Err(error) = conn.execute_batch("COMMIT") {
                    let _ = conn.execute_batch("ROLLBACK");
                    if moved_archive {
                        let _ = std::fs::rename(&tombstone, &archive_path);
                    }
                    return Err(error.into());
                }
                if moved_archive {
                    std::fs::remove_file(tombstone)?;
                }
                Ok(true)
            }
            Ok(false) => {
                let _ = conn.execute_batch("ROLLBACK");
                if moved_archive {
                    let _ = std::fs::rename(&tombstone, &archive_path);
                }
                Ok(false)
            }
            Err(error) => {
                let _ = conn.execute_batch("ROLLBACK");
                if moved_archive {
                    let _ = std::fs::rename(&tombstone, &archive_path);
                }
                Err(error)
            }
        }
    }

    fn session_archive_path(&self, session_id: &SessionId) -> StoreResult<String> {
        if !file::is_safe_filename(session_id) {
            return Err(crate::error::StoreError::InvalidArgument(format!(
                "unsafe session id for filename: {}",
                session_id.as_str()
            )));
        }
        let archive_root = self.archive_dir.canonicalize()?;
        let target = archive_root.join(format!("{}.yaml", session_id.as_str()));
        if target.parent() != Some(archive_root.as_path()) {
            return Err(crate::error::StoreError::InvalidArgument(
                "archive target escaped archive directory".into(),
            ));
        }
        Ok(target.to_string_lossy().into_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archive_boundary_rejects_trusted_but_unsafe_id() {
        let root = tempfile::tempdir().unwrap();
        let manager = ArchiveManager::new(root.path().to_path_buf());
        let unsafe_id = SessionId::from_trusted("../outside".into());
        assert!(manager.session_archive_path(&unsafe_id).is_err());
        assert!(!root.path().parent().unwrap().join("outside.yaml").exists());
    }
}
