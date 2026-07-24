//! Host repository transport requests and opening responses.

use serde::Deserialize;
use serde::Serialize;

use super::PodId;
use super::RemoteError;

/// Pod-to-guest channel carrying one Git smart-protocol operation.
pub const MUX_POD_GIT_ENDPOINT: &str = "tascarrel-pod-git-v1";
/// Guest-to-host channels serving the host's bare repository forge.
pub const MUX_GIT_HOST_ENDPOINT: &str = "tascarrel-git-host";

/// Git service requested by a pod over its shared multiplexer.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PodGitService {
    /// Reads configured upstream state through upload-pack.
    UploadPack,
    /// Stages proposed updates through receive-pack.
    ReceivePack,
}

/// Opens one Git smart-protocol stream for the authenticated pod.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PodGitRequest {
    /// Configured repository path below `/workspace`.
    pub path: String,
    /// Smart-protocol service to open.
    pub service: PodGitService,
}

/// Initial request on a host-served Git data channel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum GitHostRequest {
    /// Serves a configured host cache for a guest-side clone or fetch.
    UploadPack {
        /// Credential-bearing source used only for host policy matching.
        source: String,
        /// Whether hostd refreshes the cache from upstream before serving it.
        #[serde(default)]
        refresh: bool,
        /// Cache identity required by an internal seed fetch.
        #[serde(default)]
        expected_cache_id: Option<String>,
        /// Tracked-ref version required by an internal seed fetch.
        #[serde(default)]
        expected_version: Option<u64>,
    },
    /// Stages a pod push in an isolated namespace of a host cache.
    ReceivePack {
        /// Credential-bearing source used only by host-owned Git processes.
        source: String,
        /// Pod which initiated the staged push.
        pod_id: PodId,
        /// Configured repository path below `/workspace`.
        path: String,
    },
}

/// Response sent before a Git channel switches from JSON framing to raw bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum GitOpenResponse {
    /// An upload-pack channel is ready for raw Git protocol bytes.
    Ready,
    /// A receive-pack channel is ready and has reserved a push identifier.
    ReceivePackReady {
        /// Identifier used to retrieve the terminal policy result.
        push_id: String,
    },
    /// The Git data channel was rejected before switching to raw bytes.
    Error { error: RemoteError },
}
