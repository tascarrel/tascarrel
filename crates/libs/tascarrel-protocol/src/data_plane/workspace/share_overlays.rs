//! Revisioned copy-on-write host-share approval protocol.
//!
//! Hostd opens this guest endpoint after a user has inspected or approved an
//! exact revision. Paths and contents use Base64 so arbitrary Unix names and
//! file bytes remain unambiguous in the bounded JSON framing.

use serde::Deserialize;
use serde::Serialize;

use crate::RemoteError;

/// Host-initiated inspection or application request.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShareOverlayRequest {
    /// Pod which owns the private upper state.
    pub pod_id: String,
    /// Configured overlay share name.
    pub share: String,
    /// Requested operation.
    pub operation: ShareOverlayOperation,
}

/// Operation performed against one private upper.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum ShareOverlayOperation {
    /// Returns the current exact revision for review.
    Inspect,
    /// Freezes and prepares the exact reviewed revision for host application.
    Apply {
        /// Revision returned by the preceding inspection.
        revision: String,
    },
}

/// Guest response after preparing a coherent upper snapshot.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum ShareOverlayPrepareResponse {
    /// The requested revision is ready.
    Snapshot {
        /// Exact change set and proposed content.
        snapshot: ShareOverlaySnapshot,
    },
    /// The upper changed after it was inspected.
    RevisionChanged {
        /// New exact revision available for review.
        snapshot: ShareOverlaySnapshot,
    },
    /// The guest could not prepare the request.
    Error {
        /// Bounded failure returned to hostd.
        error: RemoteError,
    },
}

/// Host decision sent after validating an apply snapshot.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShareOverlayDecision {
    /// Host mutations completed; the guest may clear this upper revision.
    Applied,
    /// Retain every upper change because validation or host application failed.
    Retain,
}

/// Guest acknowledgement after processing the host decision.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum ShareOverlayCompletion {
    /// The decision was committed.
    Complete,
    /// The guest could not clear an applied upper.
    Error {
        /// Bounded failure returned to hostd.
        error: RemoteError,
    },
}

/// One coherent `ShareFS` upper snapshot.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShareOverlaySnapshot {
    /// SHA-256 revision of the canonical change manifest.
    pub revision: String,
    /// Changes sorted by raw path bytes.
    pub changes: Vec<ShareOverlayChange>,
}

/// One proposed path mutation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShareOverlayChange {
    /// Relative Unix path components encoded independently as Base64.
    pub path: Vec<String>,
    /// Captured lower version, or absence when the path did not exist.
    pub base: Option<ShareOverlayBase>,
    /// Proposed upper entry and content, or absence for deletion.
    pub proposed: Option<ShareOverlayEntry>,
    /// Whether a proposed directory hides all current lower children.
    pub opaque: bool,
}

/// Captured lower version and cheap host-comparable timestamps.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShareOverlayBase {
    /// Content and logical metadata captured on first override.
    pub version: ShareOverlayEntryVersion,
    /// Modification-time seconds since the Unix epoch.
    pub modified_seconds: i64,
    /// Modification-time nanoseconds.
    pub modified_nanoseconds: u32,
    /// Inode-change-time seconds since the Unix epoch.
    pub changed_seconds: i64,
    /// Inode-change-time nanoseconds.
    pub changed_nanoseconds: u32,
}

/// Proposed entry retained in the upper snapshot.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShareOverlayEntry {
    /// Content and logical metadata.
    pub version: ShareOverlayEntryVersion,
    /// Base64 regular-file bytes or symbolic-link target; absent for a
    /// directory.
    pub contents: Option<String>,
}

/// Portable entry identity used for conflict detection.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShareOverlayEntryVersion {
    /// Entry type.
    pub kind: ShareOverlayEntryKind,
    /// File length or symbolic-link target length.
    pub size: u64,
    /// Logical Unix permission bits.
    pub mode: u32,
    /// Hexadecimal SHA-256 content digest when applicable.
    pub content_digest: Option<String>,
}

/// Entry types supported by `ShareFS`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShareOverlayEntryKind {
    /// Regular file.
    File,
    /// Directory.
    Directory,
    /// Symbolic link.
    Symlink,
}

/// Host endpoint implemented by guestd for explicit overlay-share approval.
pub const MUX_SHARE_OVERLAY_GUEST_ENDPOINT: &str = "tascarrel-share-overlay-guest-v1";
/// Maximum encoded request, snapshot, decision, or acknowledgement.
pub const MAX_SHARE_OVERLAY_FRAME_LEN: usize = 64 * 1024 * 1024;
/// Maximum number of changes accepted in one approval snapshot.
pub const MAX_SHARE_OVERLAY_CHANGES: usize = 100_000;
/// Maximum decoded contents retained for one approval operation.
pub const MAX_SHARE_OVERLAY_CONTENT_BYTES: u64 = 48 * 1024 * 1024;
