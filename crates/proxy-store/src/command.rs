use proxy_common::{SessionId, TaskId};

use crate::models::ArchiveOptions;

/// Commands for batch operations.
pub enum RunCommand {
    /// Archive sessions.
    Archive {
        session_ids: Option<Vec<SessionId>>,
        options: ArchiveOptions,
    },
    /// Generate summaries for tasks.
    Summary {
        task_ids: Option<Vec<TaskId>>,
    },
}

/// Result of a RunCommand.
pub enum RunResult {
    Archive(Vec<crate::models::ArchiveInfo>),
    Summary { processed: usize },
}
