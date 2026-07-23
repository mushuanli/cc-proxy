use proxy_common::SessionId;
use std::path::Path;

use crate::error::StoreResult;

/// Write content to a temp file, then atomically rename to the target path.
pub fn atomic_write(path: &Path, content: &str) -> StoreResult<()> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("archive");
    let tmp_path = path.with_file_name(format!(".{file_name}.{}.tmp", ulid::Ulid::new()));

    std::fs::write(&tmp_path, content)?;

    // Ensure data is on disk before rename
    let f = std::fs::File::open(&tmp_path)?;
    f.sync_all()?;
    drop(f);

    if let Err(error) = std::fs::rename(&tmp_path, path) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(error.into());
    }

    Ok(())
}

/// Clean up leftover .tmp files in the archive directory.
pub fn cleanup_tmp_files(archive_dir: &Path) -> StoreResult<usize> {
    let mut count = 0;
    if archive_dir.is_dir() {
        for entry in std::fs::read_dir(archive_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("tmp") {
                std::fs::remove_file(&path)?;
                count += 1;
            }
        }
    }
    Ok(count)
}

/// Validate that a session ID is safe for use as a filename.
pub fn is_safe_filename(id: &SessionId) -> bool {
    id.as_str()
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_filename_accepts_ulid() {
        let id = SessionId::from_trusted("01JZA7M8MYP6K9X7HYF5Q2W3EN".into());
        assert!(is_safe_filename(&id));
    }

    #[test]
    fn safe_filename_rejects_slash() {
        let id = SessionId::from_trusted("bad/name".into());
        assert!(!is_safe_filename(&id));
    }
}
