//! Host-owned workspace lifecycle, inventory, and VM log streaming.
//!
//! [`WorkspaceService`] coordinates the VM runtime with resumable API state.
//! [`WorkspaceListSubscription`] exposes the mutation-driven workspace list,
//! [`WorkspaceVmLogSubscription`] exposes one VM instance's bounded log, and
//! [`UsbDeviceSubscription`] exposes live host USB inventory and forwarding
//! state.

mod log;
mod mux;
mod service;
mod usb;
mod watcher;

pub use log::WorkspaceVmLogSubscription;
pub(crate) use mux::WorkspaceEnvironmentRequest;
pub(crate) use mux::WorkspaceEnvironmentRequests;
pub(crate) use mux::WorkspaceNetworkRequest;
pub(crate) use mux::WorkspaceNetworkRequests;
pub use service::ExternalWorkspaceConfig;
pub use service::ManagedWorkspaceConfig;
pub use service::WorkspaceListSubscription;
pub use service::WorkspaceMode;
pub use service::WorkspaceService;
pub use service::WorkspaceServiceConfig;
pub use service::WorkspaceServiceError;
pub(crate) use service::create_private_directory;
pub(crate) use service::lock_file;
pub use usb::UsbDeviceSubscription;
