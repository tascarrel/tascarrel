//! Safe, uncached inspection of pod workspace files.
//!
//! [`FilesService`] resolves every request below the persistent pod workspace
//! with descriptor-relative, no-follow traversal. Directory metadata is read
//! on demand while Git annotations come from the operation-provided changes
//! service cache.

mod service;

pub use service::FileRead;
pub use service::FilesService;
pub use service::FilesServiceError;
pub(crate) use service::open_directory;
