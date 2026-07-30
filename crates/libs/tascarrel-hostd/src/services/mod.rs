//! Host-owned service implementations independent of control-plane transports.
//!
//! [`auth::AuthService`] owns browser pairing and durable sessions.
//! [`automations::AutomationService`] owns durable workspace workflow
//! admission, scheduling, execution state, and retained output.
//! [`config::ConfigService`] owns workspace configuration loading and native
//! change observation. [`host_operations::HostOperationService`] owns durable,
//! approval-gated processes executed on the physical host.
//! [`network::NetworkService`] owns host HTTP routes and TCP forwards.
//! [`repositories::RepositoryService`] exposes configured repositories and
//! workspace Git cache statistics. [`secrets::SecretsService`] resolves and
//! resolves and mutates host-owned secret providers. Share overlay approvals
//! inspect and apply guest-proposed filesystem changes.
//! [`workspaces::WorkspaceService`] owns workspace lifecycle state, resumable
//! inventory, and VM-instance logs.

pub mod auth;
pub mod automations;
pub mod config;
pub mod host_operations;
pub mod network;
pub mod repositories;
pub mod secrets;
pub(crate) mod share_overlays;
pub mod workspaces;
