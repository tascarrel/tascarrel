//! Protocols for transferring host-owned workspace inputs into a guest.

mod environment;
pub mod snapshot;

pub use environment::MAX_WORKSPACE_ENVIRONMENT_FAILURE_BYTES;
pub use environment::MAX_WORKSPACE_ENVIRONMENT_FRAME_LEN;
pub use environment::WorkspaceEnvironmentFailure;
pub use environment::WorkspaceEnvironmentResponse;

/// Guest-initiated fetch of the workspace HTTPS interception CA certificate.
pub const MUX_CA_HOST_ENDPOINT: &str = "tascarrel-ca-host-v1";
/// Guest-initiated fetch of an atomic workspace input snapshot.
pub const MUX_WORKSPACE_HOST_ENDPOINT: &str = "tascarrel-workspace-host-v1";
/// Guest-initiated fetch of the host-resolved workspace environment.
pub const MUX_WORKSPACE_ENVIRONMENT_HOST_ENDPOINT: &str = "tascarrel-workspace-environment-host-v1";
