//! Typed failure contract for the pod-local command and Git adapters.
//!
//! [`PodctlError`] keeps transport, contract, and remote-operation failures
//! distinct while [`PodctlResult`] preserves their report chains.

use reportify::Report;
use tascarrel_api::types::protocol as wire;
use tascarrel_protocol::RemoteError;
use thiserror::Error;

/// Failure while executing a podctl command or Git transport adapter.
#[derive(Debug, Error)]
pub(crate) enum PodctlError {
    /// Process-wide tracing could not be initialized.
    #[error("failed to initialize podctl logging")]
    Logging {
        /// Subscriber installation failure reported by tracing.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    /// The asynchronous runtime could not be created.
    #[error("failed to create the podctl asynchronous runtime")]
    Runtime,
    /// Git did not invoke its helper with the required arguments.
    #[error("invalid Git helper invocation")]
    InvalidGitInvocation,
    /// A Git remote URL did not use the managed Tascarrel form.
    #[error("invalid Tascarrel Git remote URL")]
    InvalidGitRemote,
    /// A Git repository path was not a normal UTF-8 path below `/workspace`.
    #[error("invalid Tascarrel Git repository path")]
    InvalidRepositoryPath,
    /// A Git helper standard stream could not be opened.
    #[error("failed to open Git helper {stream}")]
    OpenGitStream {
        /// Standard stream which could not be opened.
        stream: &'static str,
    },
    /// The transport-independent Git helper failed.
    #[error("Tascarrel Git remote helper failed")]
    GitHelper,
    /// The local adapter could not prepare its Git byte stream.
    #[error("failed to prepare the pod Git byte stream")]
    PrepareGitStream,
    /// The local Git process stopped before its bridge became ready.
    #[error("pod Git bridge stopped before accepting the operation")]
    GitBridgeStopped,
    /// The Git bridge could not report its readiness to the caller.
    #[error("Git helper stopped before the pod Git bridge became ready")]
    GitHelperStopped,
    /// The pod Git endpoint rejected the requested operation.
    #[error("pod Git operation was rejected")]
    GitRejected(#[source] RemoteError),
    /// The pod Git endpoint returned a response for a different service.
    #[error("pod Git endpoint returned an invalid service response")]
    InvalidGitResponse,
    /// The pod Git endpoint closed before accepting the request.
    #[error("pod Git endpoint closed before accepting the operation")]
    GitConnectionClosed,
    /// The framed Git opening handshake failed.
    #[error("pod Git opening handshake failed")]
    GitHandshake,
    /// Git request bytes could not be relayed to guestd.
    #[error("failed to relay the pod Git request")]
    RelayGitRequest,
    /// Git response bytes could not be relayed from guestd.
    #[error("failed to relay the pod Git response")]
    RelayGitResponse,
    /// Guestd did not complete the Git channel close handshake.
    #[error("failed to close the pod Git channel")]
    CloseGitChannel,
    /// Receive-pack bytes could not be relayed through guestd.
    #[error("failed to relay the pod Git receive-pack protocol")]
    RelayReceivePack,
    /// Host policy rejected at least one ref in an atomic push.
    #[error("Git push denied: {0}")]
    GitPushDenied(String),
    /// A user rejected the pending publication approval.
    #[error("Git push rejected")]
    GitPushRejected,
    /// Automatic upstream publication failed after receive-pack completed.
    #[error("Git push publication failed: {0}")]
    GitPushFailed(String),
    /// The pod-local guestd socket could not be connected.
    #[error("failed to connect to the pod control socket")]
    ConnectControlSocket,
    /// The Tascarrel multiplexer could not be configured or driven.
    #[error("pod multiplexer operation failed")]
    Multiplexer,
    /// The typed control plane could not be configured or driven.
    #[error("pod control-plane operation failed")]
    ControlPlane,
    /// Guestd closed the control channel before assigning socket identity.
    #[error("pod control channel closed before assigning identity")]
    IdentityUnavailable,
    /// A typed control request could not be encoded.
    #[error("failed to encode a pod control request")]
    InvalidControlInput,
    /// A typed control response could not be decoded.
    #[error("failed to decode a pod control response")]
    InvalidControlOutput,
    /// Guestd returned a peer-visible operation failure.
    #[error("pod control request failed")]
    RemoteOperation(#[source] wire::OperationError),
    /// A control-plane action was canceled before completion.
    #[error("pod control action was canceled")]
    ActionCanceled,
    /// A control-plane subscription completed without an initial event.
    #[error("pod control subscription completed before its initial event")]
    SubscriptionCompleted,
    /// The control plane returned a message invalid for the current operation.
    #[error("pod control plane returned an invalid response")]
    InvalidControlResponse,
    /// Guestd closed the control-plane connection during an operation.
    #[error("pod control-plane connection closed")]
    ControlConnectionClosed,
    /// A live-state subscription did not begin with a snapshot.
    #[error("invalid {resource} subscription: the initial event is not a snapshot")]
    InitialEventNotSnapshot {
        /// Human-readable live resource name.
        resource: &'static str,
    },
    /// JSON command output could not be encoded.
    #[error("failed to encode podctl JSON output")]
    EncodeOutput,
    /// Command output could not be written.
    #[error("failed to write podctl output")]
    WriteOutput,
    /// Port zero is not a usable pod service port.
    #[error("invalid pod port: expected a value in 1..=65535")]
    InvalidPort,
    /// The selected pod port has no dynamic host forward.
    #[error("pod port is not published")]
    PortNotPublished,
    /// The selected HTTP route does not exist for this pod.
    #[error("HTTP route does not exist")]
    HttpRouteNotFound,
    /// HTTP route creation and its compensating forward deletion both failed.
    #[error("failed to create an HTTP route and roll back its port forward")]
    PortForwardRollback,
    /// A guestd device-helper path escaped its permitted tree.
    #[error("invalid {kind}")]
    InvalidDevicePath {
        /// Kind of path rejected by the helper.
        kind: &'static str,
    },
    /// A path occupied by a device link is a directory.
    #[error("invalid device path: the target is a directory")]
    DevicePathIsDirectory,
    /// A device-link parent was absent or escaped `/dev`.
    #[error("invalid device-link parent")]
    InvalidDeviceParent,
    /// A device-link parent component was not a real directory.
    #[error("invalid device-link parent: a component is not a real directory")]
    UnsafeDeviceParent,
    /// A filesystem operation used by the device helper failed.
    #[error("failed to {action}")]
    DeviceIo {
        /// Filesystem operation which failed.
        action: &'static str,
    },
}

/// Result returned by podctl command and transport operations.
pub(crate) type PodctlResult<T> = Result<T, Report<PodctlError>>;
