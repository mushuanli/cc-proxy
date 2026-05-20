use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::response::IntoResponse;
use axum::Json;
use serde_json::json;

use crate::AppState;

/// Transform ArchiveInfo into frontend-compatible JSON (file, name, last_active_at, size).
fn archive_to_json(a: &proxy_store::ArchiveInfo) -> serde_json::Value {
    let path = std::path::Path::new(&a.file_path);
    let file = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_string();
    let size = path.metadata().map(|m| m.len()).unwrap_or(0);
    // Try to read the YAML file for name and last_activity_at — fall back to file mtime
    let mut name = a.name.clone();
    let mut last_active_at: Option<String> = None;
    if let Ok(contents) = std::fs::read_to_string(path) {
        // Extract name and last_activity_at from YAML header without a full parser
        for line in contents.lines().take(60) {
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
    }
    // Fallback: use file modification time
    if last_active_at.is_none() {
        last_active_at = path.metadata().ok().and_then(|m| m.modified().ok()).and_then(|t| {
            let dur = t.duration_since(std::time::UNIX_EPOCH).ok()?;
            chrono::DateTime::from_timestamp(dur.as_secs() as i64, 0)
                .map(|dt| dt.to_rfc3339())
        });
    }
    json!({
        "file": file,
        "name": name,
        "last_active_at": last_active_at,
        "size": size,
        "snippets": [],
        "match_count": 0,
    })
}

pub async fn list(
    State(state): State<Arc<AppState>>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    let filter = q.get("q").map(|s| s.as_str());
    match state.store.list_archives(filter) {
        Ok(archives) => {
            let items: Vec<serde_json::Value> = archives
                .iter()
                .map(archive_to_json)
                .collect();
            Json(json!(items)).into_response()
        }
        Err(e) => Json(json!({"error": e.to_string()})).into_response(),
    }
}

pub async fn search(
    State(state): State<Arc<AppState>>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    let filter = q.get("q").map(|s| s.as_str());
    match state.store.list_archives(filter) {
        Ok(archives) => {
            let items: Vec<serde_json::Value> = archives
                .iter()
                .map(archive_to_json)
                .collect();
            Json(json!(items)).into_response()
        }
        Err(e) => Json(json!({"error": e.to_string()})).into_response(),
    }
}

pub async fn file(
    Path(_name): Path<String>,
) -> impl IntoResponse {
    Json(json!({"error": "not implemented"})).into_response()
}

pub async fn set_name(
    Path(_sid): Path<String>,
    Json(_body): Json<serde_json::Value>,
) -> impl IntoResponse {
    Json(json!({"ok": false, "error": "not implemented"})).into_response()
}
