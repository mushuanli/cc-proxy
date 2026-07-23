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
    pub fn search(&self, query: &str, role_filter: Option<&str>) -> StoreResult<Vec<ArchiveSearchResult>> {
        let mut results = Vec::new();
        if !self.archive_dir.is_dir() {
            return Ok(results);
        }

        let q_lower = query.to_lowercase();

        for entry in std::fs::read_dir(&self.archive_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("yaml") {
                continue;
            }

            let content = match std::fs::read_to_string(&path) {
                Ok(c) => c,
                Err(_) => continue,
            };

            let file = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string();
            let size = path.metadata().map(|m| m.len()).unwrap_or(0);

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
                if let Some(ref rf) = role_filter {
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
                        } else if line.starts_with("  name:") {
                            false
                        } else {
                            true
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

    fn session_archive_path(&self, session_id: &SessionId) -> String {
        self.archive_dir
            .join(format!("{}.yaml", session_id.as_str()))
            .to_string_lossy()
            .into_owned()
    }
}
