use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use proxy_common::EventBus;
use proxy_common::WsMessage;

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
        self.events.publish(WsMessage::TeeStatusChanged { enabled: val });
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
}
