//! Host-owned workspace configuration loading and change observation.
//!
//! [`ConfigService`] reads typed workspace configuration snapshots and owns a
//! native filesystem watcher. [`ConfigSubscription`] exposes debounced current
//! state without coupling the service to a control-plane transport, while
//! [`ConfigServiceConfig`] defines its filesystem and resource bounds.

mod service;
mod settings;
mod snapshot;
mod watcher;

pub use service::ConfigService;
pub use service::ConfigServiceConfig;
pub use service::ConfigServiceError;
pub use service::ConfigSubscription;
pub(crate) use service::DEFAULT_MAX_CONFIG_BYTES;
pub(crate) use snapshot::decode_config;
pub(crate) use snapshot::load_config_file;
