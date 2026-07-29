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
    /// Exact host cache identity required by an internal pod materialization.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_cache_id: Option<String>,
    /// Exact tracked-ref version required by an internal pod materialization.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_version: Option<u64>,
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
    /// An exact cache version is ready for workspace-seed materialization.
    VersionedReady {
        /// Branch selected by the cached upstream symbolic `HEAD`, when
        /// advertised.
        default_branch: Option<String>,
    },
    /// A receive-pack channel is ready and has reserved a push identifier.
    ReceivePackReady {
        /// Identifier used to retrieve the terminal policy result.
        push_id: String,
    },
    /// The Git data channel was rejected before switching to raw bytes.
    Error { error: RemoteError },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Keeps ordinary pod Git clients compatible with requests that predate
    /// exact cache-version selection.
    #[test]
    fn pod_git_request_defaults_exact_cache_fields() {
        let request: PodGitRequest =
            serde_json::from_str(r#"{"path":"repository","service":"upload_pack"}"#).unwrap();
        assert_eq!(
            request,
            PodGitRequest {
                path: "repository".to_owned(),
                service: PodGitService::UploadPack,
                expected_cache_id: None,
                expected_version: None,
            }
        );
        assert_eq!(
            serde_json::to_string(&request).unwrap(),
            r#"{"path":"repository","service":"upload_pack"}"#
        );
    }
}
