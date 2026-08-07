//! Safe, uncached access to pod-visible file roots.
//!
//! [`FilesService`] resolves requests below the persistent workspace or a
//! configured host share with no-follow traversal. Directory metadata is read
//! on demand, file bodies can be streamed or revision-safely replaced, and
//! workspace Git annotations come from the operation-provided changes service
//! cache.

mod service;

pub use service::FileRead;
pub use service::FileWrite;
pub use service::FilesService;
pub use service::FilesServiceConfig;
pub use service::FilesServiceError;
pub(crate) use service::open_directory;
