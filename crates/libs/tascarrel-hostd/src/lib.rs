//! Host-side workspace lifecycle and connection routing.
//!
//! [`Broker`] serves the host control plane on one private per-user socket.
//! [`GuestClient`] reaches operations owned by workspace guest daemons through
//! the same typed routing layer, while [`HostState`] composes the long-lived
//! services shared by local and web API links.

pub mod authority;
pub mod broker;
mod control_plane;
pub mod daemon;
pub mod guest;
pub mod paths;
mod server_config;
pub mod services;
pub mod socket;
pub mod startup;
pub mod web;

pub use authority::WorkspaceAuthority;
pub use broker::Broker;
pub use broker::BrokerError;
pub use control_plane::HostControlService;
pub use control_plane::HostState;
pub use guest::GuestClient;
pub use guest::GuestClientError;
pub use guest::GuestEventStream;
pub use guest::GuestResult;
pub use paths::TascarrelHome;
pub use paths::TascarrelHomeError;
pub use services::network::NetworkProxyError;
pub use services::network::NetworkService;
pub use services::network::NetworkServiceConfig;
pub use services::network::NetworkServiceError;
pub use services::repositories::HostRepositoryManager;
pub use services::repositories::RepositoryService;
pub use services::repositories::RepositoryServiceConfig;
pub use services::repositories::RepositoryServiceError;
pub use services::workspaces::ExternalWorkspaceConfig;
pub use services::workspaces::ManagedWorkspaceConfig;
pub use services::workspaces::WorkspaceMode;
pub use services::workspaces::WorkspaceService;
pub use services::workspaces::WorkspaceServiceConfig;
pub use socket::bind_control_socket;
pub use socket::remove_control_socket;
pub use startup::StartupFailure;
pub use startup::StartupReporter;
pub use startup::server_issue;
