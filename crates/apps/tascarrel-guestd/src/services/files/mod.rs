//! Safe, uncached inspection of pod-visible file roots.
//!
//! [`FilesService`] resolves requests below the persistent workspace or a
//! configured host share with no-follow traversal. Directory metadata is read
//! on demand while workspace Git annotations come from the operation-provided
//! changes service cache.

mod service;

pub use service::FileRead;
pub use service::FilesService;
pub use service::FilesServiceConfig;
pub use service::FilesServiceError;
pub(crate) use service::open_directory;
