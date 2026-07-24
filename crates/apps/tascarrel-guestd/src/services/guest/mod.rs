//! Guest system information and bounded resource metric collection.
//!
//! [`GuestService`] owns one long-lived system sampler and a resumable
//! in-memory metric history. Control-plane dispatch and daemon construction
//! remain outside this feature.

mod service;

pub(crate) use service::GuestMetricsSubscription;
pub use service::GuestService;
pub use service::GuestServiceConfig;
pub use service::GuestServiceError;
