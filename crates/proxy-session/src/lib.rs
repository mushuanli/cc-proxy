pub mod archive;
pub mod domain;
pub mod ingest;
pub mod persist;
pub mod pipeline;
pub mod query;
pub mod source;

pub use domain::model_call::ModelCallRow;
pub use domain::status::{CallStatus, ToolStatus};
pub use domain::tool_invocation::ToolInvocationRow;
pub use error::{SessionError, SessionResult};
pub use ingest::observation::{Observation, ObservationKind, TokenUsage};
pub use ingest::{SessionIngest, SessionIngestExt};
pub use persist::repo::{SessionRepo, SessionRepoConfig};
pub use query::{TimelineDocument, TimelineReader};
pub use source::{AnthropicParser, CodexParser, HeuristicClassifier, HookParser, OtelParser};

mod error {
    use thiserror::Error;

    /// Errors produced by the proxy-session crate.
    #[derive(Debug, Error)]
    pub enum SessionError {
        #[error("database error: {0}")]
        Database(#[from] rusqlite::Error),
        #[error("invalid argument: {0}")]
        InvalidArgument(String),
        #[error("not found: {0}")]
        NotFound(String),
        #[error("serialization error: {0}")]
        Serialization(String),
    }

    pub type SessionResult<T> = Result<T, SessionError>;
}
