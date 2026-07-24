//! Identities exchanged when authenticated guest and pod links are established.
//!
//! [`GuestControlIdentity`] establishes the workspace on the trusted host link,
//! while [`PodControlIdentity`] reports the identity assigned to a pod
//! listener.

use serde::Deserialize;
use serde::Serialize;
use tascarrel_api::types::pods::PodId;
use tascarrel_api::types::workspaces::WorkspaceName;

/// Identity of the workspace carried by an authenticated host-to-guest link.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GuestControlIdentity {
    /// Workspace assigned to the guest multiplexer by hostd.
    pub workspace: WorkspaceName,
}

/// Identity assigned to one authenticated pod control socket.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PodControlIdentity {
    /// Workspace containing the pod.
    pub workspace: WorkspaceName,
    /// Pod bound to the socket.
    pub pod_id: PodId,
}
