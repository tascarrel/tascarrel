//! Live pod repository status and exact Git change inspection.
//!
//! [`ChangesService`] owns fanotify invalidation, bounded Git subprocesses,
//! per-repository status overlays, and the resumable workspace repository
//! inventory. Pod and repository services are supplied when operations begin
//! rather than retained as construction dependencies.

mod git;
mod service;
mod watcher;

pub use service::ChangesService;
pub use service::ChangesServiceConfig;
pub use service::ChangesServiceError;
pub(crate) use service::RepositoryStatusListSubscription;
