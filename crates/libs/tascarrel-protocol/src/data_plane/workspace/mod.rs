//! Protocols for host-owned workspace inputs and guest-owned share proposals.

mod environment;
mod share_overlays;
mod shares;
pub mod snapshot;

pub use environment::MAX_WORKSPACE_ENVIRONMENT_FAILURE_BYTES;
pub use environment::MAX_WORKSPACE_ENVIRONMENT_FRAME_LEN;
pub use environment::WorkspaceEnvironmentFailure;
pub use environment::WorkspaceEnvironmentResponse;
pub use share_overlays::MAX_SHARE_OVERLAY_CHANGES;
pub use share_overlays::MAX_SHARE_OVERLAY_CONTENT_BYTES;
pub use share_overlays::MAX_SHARE_OVERLAY_FRAME_LEN;
pub use share_overlays::MUX_SHARE_OVERLAY_GUEST_ENDPOINT;
pub use share_overlays::ShareOverlayBase;
pub use share_overlays::ShareOverlayChange;
pub use share_overlays::ShareOverlayCompletion;
pub use share_overlays::ShareOverlayDecision;
pub use share_overlays::ShareOverlayEntry;
pub use share_overlays::ShareOverlayEntryKind;
pub use share_overlays::ShareOverlayEntryVersion;
pub use share_overlays::ShareOverlayOperation;
pub use share_overlays::ShareOverlayPrepareResponse;
pub use share_overlays::ShareOverlayRequest;
pub use share_overlays::ShareOverlaySnapshot;
pub use shares::MAX_WORKSPACE_HOST_SHARES;
pub use shares::MAX_WORKSPACE_SHARE_MOUNT_TAG_BYTES;
pub use shares::MAX_WORKSPACE_SHARE_NAME_BYTES;
pub use shares::MAX_WORKSPACE_SHARES_FRAME_LEN;
pub use shares::WorkspaceHostShare;
pub use shares::WorkspaceHostShareMode;
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
