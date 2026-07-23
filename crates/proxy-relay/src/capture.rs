use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use chrono::Utc;
use proxy_common::EventBus;
use proxy_common::WsMessage;

/// Data for a proxied exchange to be recorded to capture files.
#[derive(Clone)]
pub struct ExchangeInfo {
    pub method: String,
    pub path: String,
    pub status_code: u16,
    pub request_body: String,
    pub response_body: String,
    pub duration_ms: u64,
}

/// Capture controller — toggles request/response recording to disk.
///
/// When enabled, writes every proxied exchange to `captures/YYYY-MM-DD/session_<id>.txt`.
/// Status changes are broadcast via EventBus so the frontend stays in sync.
#[derive(Clone)]
pub struct CaptureControl {
    enabled: Arc<AtomicBool>,
    output_dir: PathBuf,
    events: EventBus,
}

impl CaptureControl {
    pub fn new(output_dir: PathBuf, events: EventBus) -> Self {
        Self {
            enabled: Arc::new(AtomicBool::new(false)),
            output_dir,
            events,
        }
    }

    /// Toggle recording on/off. Broadcasts TeeStatusChanged.
    pub fn set_enabled(&self, val: bool) {
        self.enabled.store(val, Ordering::Relaxed);
        self.events
            .publish(WsMessage::TeeStatusChanged { enabled: val });
    }

    /// Query whether recording is active.
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    /// Return the output directory path.
    pub fn output_dir(&self) -> &PathBuf {
        &self.output_dir
    }

    /// Return a clone of the enabled flag (for TeeWriter).
    pub fn enabled_flag(&self) -> Arc<AtomicBool> {
        self.enabled.clone()
    }

    /// Record a proxied request/response exchange to disk.
    /// Creates `captures/YYYY-MM-DD/session_<sid>.txt` with timestamped entries.
    pub fn record_exchange(&self, session_id: &str, info: &ExchangeInfo) {
        if !self.is_enabled() {
            return;
        }

        let output_dir = self.output_dir.clone();
        let session_id = session_id.to_string();
        let info = info.clone();
        tokio::task::spawn_blocking(move || {
            let date_dir = Utc::now().format("%Y-%m-%d").to_string();
            let session_dir = output_dir.join(&date_dir);
            if let Err(e) = std::fs::create_dir_all(&session_dir) {
                tracing::warn!(
                    "[capture] failed to create dir {}: {}",
                    session_dir.display(),
                    e
                );
                return;
            }

            let safe_sid: String = session_id
                .chars()
                .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
                .take(64)
                .collect();
            let file_path = session_dir.join(format!("session_{}.txt", safe_sid));

            let timestamp = Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ");
            let request_body = truncate_capture(&info.request_body);
            let response_body = truncate_capture(&info.response_body);
            let entry = format!(
            "---\ntimestamp: {}\nmethod: {}\npath: {}\nstatus: {}\nduration_ms: {}\n\n> REQUEST\n{}\n\n> RESPONSE\n{}\n",
            timestamp, info.method, info.path, info.status_code, info.duration_ms, request_body, response_body
        );

            if let Err(e) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&file_path)
                .and_then(|mut f| {
                    use std::io::Write;
                    f.write_all(entry.as_bytes())
                })
            {
                tracing::warn!("[capture] failed to write {}: {}", file_path.display(), e);
            }
        });
    }
}

fn truncate_capture(value: &str) -> String {
    const LIMIT: usize = 1024 * 1024;
    if value.len() <= LIMIT {
        return value.to_string();
    }
    let mut end = LIMIT;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[truncated]", &value[..end])
}
