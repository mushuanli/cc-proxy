use rusqlite::Connection;
use std::path::Path;

use crate::error::StoreResult;

/// Open (or create) the SQLite database with recommended PRAGMAs.
pub fn open_database(path: &Path) -> StoreResult<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let conn = Connection::open(path)?;

    conn.execute_batch(
        "
        PRAGMA journal_mode = WAL;
        PRAGMA foreign_keys = ON;
        PRAGMA synchronous = NORMAL;
        PRAGMA busy_timeout = 5000;
        ",
    )?;

    Ok(conn)
}
