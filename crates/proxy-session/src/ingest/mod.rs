//! Event ingestion: the single entry point for raw observations.

use crate::SessionResult;

pub mod observation;

pub use observation::{Observation, ObservationKind, TokenUsage};

/// Receives raw observations appended to the event store.
///
/// Relay, hook receiver, and OTel collector all produce `Observation`s
/// through this trait. Records are idempotent by `event_id`.
pub trait SessionIngest: Send + Sync {
    /// Append a single observation. Duplicate `event_id` is ignored.
    fn record(&self, obs: Observation) -> SessionResult<()>;

    /// Append multiple observations in order.
    fn record_many(&self, obs: &[Observation]) -> SessionResult<()> {
        for o in obs {
            self.record(o.clone())?;
        }
        Ok(())
    }
}

/// Convenience blanket impl so callers can pass an `Option<Arc<dyn SessionIngest>>`
/// and record only when a collector is configured.
pub trait SessionIngestExt {
    fn record_if_any(&self, obs: Observation);
}

impl SessionIngestExt for Option<std::sync::Arc<dyn SessionIngest>> {
    fn record_if_any(&self, obs: Observation) {
        if let Some(ingest) = self {
            if let Err(e) = ingest.record(obs) {
                tracing::warn!("[session] failed to record observation: {}", e);
            }
        }
    }
}
