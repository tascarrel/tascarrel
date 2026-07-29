//! Host-owned service implementations independent of control-plane transports.
//!
//! [`auth::AuthService`] owns browser pairing and durable sessions.
//! [`config::ConfigService`] owns workspace configuration loading and native
//! change observation. [`host_operations::HostOperationService`] owns durable,
//! approval-gated processes executed on the physical host.
//! [`network::NetworkService`] owns host HTTP routes and TCP forwards.
//! [`repositories::RepositoryService`] exposes configured repositories and
//! workspace Git cache statistics. [`secrets::SecretsService`] resolves and
//! mutates host-owned secret providers.
//! [`workspaces::WorkspaceService`] owns workspace lifecycle state, resumable
//! inventory, and VM-instance logs.

pub mod auth;
pub mod config;
pub mod host_operations;
pub mod network;
pub mod repositories;
pub mod secrets;
pub mod workspaces;
