//! Protocols for transferring host-owned workspace inputs into a guest.

mod environment;
mod shares;
pub mod snapshot;

pub use environment::MAX_WORKSPACE_ENVIRONMENT_FAILURE_BYTES;
pub use environment::MAX_WORKSPACE_ENVIRONMENT_FRAME_LEN;
pub use environment::WorkspaceEnvironmentFailure;
pub use environment::WorkspaceEnvironmentResponse;
pub use shares::MAX_WORKSPACE_HOST_SHARES;
pub use shares::MAX_WORKSPACE_SHARE_MOUNT_TAG_BYTES;
pub use shares::MAX_WORKSPACE_SHARE_NAME_BYTES;
pub use shares::MAX_WORKSPACE_SHARES_FRAME_LEN;
pub use shares::WorkspaceHostShare;
pub use shares::WorkspaceHostSharesMessageError;
pub use shares::WorkspaceHostSharesResponse;
pub use shares::valid_workspace_share_name;

/// Guest-initiated fetch of the workspace HTTPS interception CA certificate.
pub const MUX_CA_HOST_ENDPOINT: &str = "tascarrel-ca-host-v1";
/// Guest-initiated fetch of an atomic workspace input snapshot.
pub const MUX_WORKSPACE_HOST_ENDPOINT: &str = "tascarrel-workspace-host-v1";
/// Guest-initiated fetch of the host-resolved workspace environment.
pub const MUX_WORKSPACE_ENVIRONMENT_HOST_ENDPOINT: &str = "tascarrel-workspace-environment-host-v1";
/// Guest-initiated fetch of host directories pinned to the current VM.
pub const MUX_WORKSPACE_SHARES_HOST_ENDPOINT: &str = "tascarrel-workspace-shares-host-v1";
