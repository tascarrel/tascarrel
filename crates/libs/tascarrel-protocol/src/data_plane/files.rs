//! Protocol for streaming one pod workspace file.
//!
//! [`WorkspaceFileReadRequest`] selects a file through the Files API path
//! contract. [`WorkspaceFileReadResponse`] precedes raw bytes on the same
//! multiplexed channel.

use serde::Deserialize;
use serde::Serialize;
use tascarrel_api::types::files::FilePath;
use tascarrel_api::types::pods::PodId;

/// Host-to-guest request followed by a guest-to-host workspace file stream.
pub const MUX_WORKSPACE_FILE_READ_ENDPOINT: &str = "tascarrel-workspace-file-read-v1";

/// Requests one normalized workspace-relative file from guestd.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkspaceFileReadRequest {
    /// Pod containing the requested file.
    pub pod_id: PodId,
    /// UTF-8 path relative to the pod's `/workspace` directory.
    pub path: FilePath,
}

/// Metadata frame preceding a successful raw file stream.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "status", rename_all = "camelCase")]
pub enum WorkspaceFileReadResponse {
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
