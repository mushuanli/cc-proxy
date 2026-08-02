//! ArchiveV4: persistent session timeline snapshots.
//!
//! The store crate owns the atomic write flow; this module defines the
//! serialization format and the read path for timeline reconstruction.

pub mod v4;

pub use v4::{ArchiveV4, ArchiveV4Segment, ArchiveTimeline, read_archive_v4};
