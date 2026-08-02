//! ArchiveV4 timeline segments (version 4).
//!
//! Segment files hold a full session timeline snapshot so that a cleaned-up
//! session can still render its task history. Written atomically by the store
//! crate; read here for timeline reconstruction.

use serde::{Deserialize, Serialize};

use crate::query::timeline::TimelineDocument;
use crate::SessionResult;

/// A timeline archive segment for one session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchiveTimeline {
    pub archived_through_sequence_no: i64,
    pub timeline: TimelineDocument,
}

/// Top-level archive v4 document (mirrors store's ArchiveDocument shape).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchiveV4 {
    pub version: u32,
    pub session_id: String,
    pub archived_through_sequence_no: i64,
    pub timeline: TimelineDocument,
}

/// One archived segment on disk (used for multi-segment merge).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchiveV4Segment {
    pub segment_seq: u64,
    pub archived_through_sequence_no: i64,
    pub document: ArchiveV4,
}

/// Read an ArchiveV4 document from YAML.
pub fn read_archive_v4(yaml: &str) -> SessionResult<ArchiveV4> {
    serde_yaml::from_str::<ArchiveV4>(yaml)
        .map_err(|e| crate::SessionError::Serialization(e.to_string()))
}

/// Serialize an ArchiveV4 document to YAML.
pub fn write_archive_v4(doc: &ArchiveV4) -> SessionResult<String> {
    serde_yaml::to_string(doc)
        .map_err(|e| crate::SessionError::Serialization(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::timeline::TimelineDocument;

    #[test]
    fn archive_roundtrip() {
        let doc = ArchiveV4 {
            version: 4,
            session_id: "sess-1".into(),
            archived_through_sequence_no: 10,
            timeline: TimelineDocument {
                session_id: "sess-1".into(),
                total_model_calls: 1,
                user_interactions: 1,
                interactions: vec![],
                summary: None,
            },
        };
        let yaml = write_archive_v4(&doc).unwrap();
        let back = read_archive_v4(&yaml).unwrap();
        assert_eq!(back.version, 4);
        assert_eq!(back.session_id, "sess-1");
        assert_eq!(back.timeline.total_model_calls, 1);
    }
}
