//! Asynchronous workspace image generation and observable image state.
//!
//! [`ImageService`] owns the host-backed input fingerprint, singleton build
//! lifecycle, SQLite inventory, Btrfs image store, build network, resumable
//! image list, and retained generation logs. Control-plane dispatch and daemon
//! construction remain outside this feature.

mod builder;
mod input;
mod log;
mod service;
mod state;

pub use builder::ImageBuildError;
pub use builder::ImageBuildLimits;
pub use builder::ImageBuildOutcome;
pub use builder::ImageBuilder;
pub use builder::ImageBuilderConfig;
pub use builder::cleanup_stale_image_build_directories;
pub(crate) use log::LogSubscription;
pub(crate) use service::ImageForPod;
pub use service::ImageInputRefresh;
pub(crate) use service::ImageListSubscription;
pub use service::ImageService;
pub use service::ImageServiceConfig;
pub use service::ImageServiceError;
