//! Protocol for reading and replacing files below pod-visible logical roots.
//!
//! [`PodFileReadRequest`] selects a root and file through the Files API path
//! contract. [`PodFileReadResponse`] precedes raw bytes on the same multiplexed
//! channel. [`PodFileWriteRequest`] precedes bounded UTF-8 replacement content,
//! and [`PodFileWriteResponse`] reports the revision produced by the write.

use serde::Deserialize;
use serde::Serialize;
use tascarrel_api::types::files::FilePath;
use tascarrel_api::types::files::FileRoot;
use tascarrel_api::types::pods::PodId;

/// Host-to-guest request followed by a guest-to-host pod file stream.
pub const MUX_POD_FILE_READ_ENDPOINT: &str = "tascarrel-pod-file-read-v1";
/// Host-to-guest metadata and content followed by a guest-to-host result.
pub const MUX_POD_FILE_WRITE_ENDPOINT: &str = "tascarrel-pod-file-write-v1";
/// Maximum UTF-8 byte length accepted for one browser file replacement.
pub const MAX_POD_FILE_WRITE_BYTES: u64 = 2 * 1024 * 1024;

/// Requests one normalized root-relative pod file from guestd.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PodFileReadRequest {
    /// Pod containing the requested file.
    pub pod_id: PodId,
    /// Logical root containing the requested file.
    pub root: FileRoot,
    /// UTF-8 path relative to the selected root.
    pub path: FilePath,
}

/// Metadata frame preceding a successful raw file stream.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "status", rename_all = "camelCase")]
pub enum PodFileReadResponse {
    /// Raw file bytes follow this frame until channel EOF.
    Found {
        /// Exact byte length observed on the pinned file handle.
        size: u64,
        /// Whether the selected root accepts file replacements.
        writable: bool,
    },
    /// The requested file could not be streamed.
    Rejected {
        /// Stable file-service failure category.
        code: String,
        /// Human-readable diagnostic safe to expose to the caller.
        message: String,
    },
}

/// Describes one revision-checked complete-file replacement.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PodFileWriteRequest {
    /// Pod containing the file to replace.
    pub pod_id: PodId,
    /// Logical root containing the file.
    pub root: FileRoot,
    /// UTF-8 path relative to the selected root.
    pub path: FilePath,
    /// Lowercase hexadecimal SHA-256 revision observed by the browser.
    pub expected_revision: String,
}

/// Result returned after replacement content has been consumed.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "status", rename_all = "camelCase")]
pub enum PodFileWriteResponse {
    /// The complete file was replaced.
    Written {
        /// Lowercase hexadecimal SHA-256 revision of the new contents.
        revision: String,
    },
    /// The file no longer matches the revision observed by the browser.
    Conflict,
    /// The replacement was rejected without changing the file.
    Rejected {
        /// Stable file-service failure category.
        code: PodFileWriteRejectionCode,
        /// Human-readable diagnostic safe to expose to the caller.
        message: String,
    },
}

/// Stable rejection categories for pod file replacements.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PodFileWriteRejectionCode {
    /// The root, path, revision, or content violates the request contract.
    InvalidRequest,
    /// The selected file root does not accept replacements.
    ReadOnly,
    /// The replacement exceeds the browser editor limit.
    TooLarge,
    /// The selected pod or file is temporarily unavailable.
    Unavailable,
    /// The guest could not complete the replacement.
    Internal,
}

impl PodFileWriteRejectionCode {
    /// Returns the stable HTTP error code spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidRequest => "invalid_request",
            Self::ReadOnly => "read_only",
            Self::TooLarge => "too_large",
            Self::Unavailable => "unavailable",
            Self::Internal => "internal",
        }
    }
}
