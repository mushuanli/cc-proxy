//! Reconciler pipeline: idempotently materialize observations into the
//! domain tables (interactions, execution_runs, agent_identities, agent_runs).

pub mod priority;
pub mod reconciler;

pub use reconciler::{Reconciler, ReconcilerConfig};
