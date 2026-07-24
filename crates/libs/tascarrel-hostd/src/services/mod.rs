//! Host-owned service implementations independent of control-plane transports.
//!
//! [`config::ConfigService`] owns workspace configuration loading and native
//! change observation. [`network::NetworkService`] owns host HTTP routes and
//! TCP forwards. [`repositories::RepositoryService`] exposes configured
//! repositories and workspace Git cache statistics.
//! [`secrets::SecretsService`] resolves and mutates host-owned secret
//! providers. [`workspaces::WorkspaceService`] owns workspace lifecycle state,
//! resumable inventory, and VM-instance logs.

pub mod config;
pub mod network;
pub mod repositories;
pub mod secrets;
pub mod workspaces;
