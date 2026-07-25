use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: migrate_db <database-path>")?;
    let conn = proxy_store::db::connection::open_database(&path)?;
    proxy_store::db::migration::migrate(&conn)?;
    conn.execute_batch("PRAGMA optimize;")?;
    println!("migrated {}", path.display());
    Ok(())
}
