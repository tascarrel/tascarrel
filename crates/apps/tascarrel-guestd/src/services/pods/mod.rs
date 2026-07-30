//! Durable pod lifecycle and resumable workspace pod inventory.
//!
//! [`PodService`] owns pod state and coordinates `SQLite`, `Btrfs` storage,
//! per-pod `ShareFS` overlays, `runc`, networking, and egress. Control-plane
//! dispatch remains outside this module.

mod nix_roots;
pub(crate) mod runc;
mod service;
mod share_overlays;
mod state;

pub(crate) use runc::PodExecution;
pub use runc::RuncConfig;
pub use runc::WorkspaceCaConfig;
pub(crate) use service::EphemeralPod;
pub use service::PodControlConnection;
pub use service::PodInitStep;
pub use service::PodInitStepError;
pub(crate) use service::PodListSubscription;
pub use service::PodService;
pub use service::PodServiceConfig;
pub use service::PodServiceError;
pub use service::PreparedPodShareOverlay;
pub use share_overlays::ShareOverlay;
pub use share_overlays::ShareOverlayRuntimeConfig;
