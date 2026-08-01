//! Protocol for streaming one file from a pod-visible logical root.
//!
//! [`PodFileReadRequest`] selects a root and file through the Files API path
//! contract. [`PodFileReadResponse`] precedes raw bytes on the same multiplexed
//! channel.

use serde::Deserialize;
use serde::Serialize;
use tascarrel_api::types::files::FilePath;
use tascarrel_api::types::files::FileRoot;
use tascarrel_api::types::pods::PodId;

/// Host-to-guest request followed by a guest-to-host pod file stream.
pub const MUX_POD_FILE_READ_ENDPOINT: &str = "tascarrel-pod-file-read-v1";

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
    },
    /// The requested file could not be streamed.
    Rejected {
        /// Stable file-service failure category.
        code: String,
        /// Human-readable diagnostic safe to expose to the caller.
        message: String,
    },
}
