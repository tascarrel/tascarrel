//! Typed failures produced by copy-on-write share operations.
//!
//! [`ShareFsError`] exposes the conditions callers can handle, while
//! [`ShareFsResult`] retains diagnostic context through `reportify`.

use std::io;
use std::path::PathBuf;

use reportify::Report;
use thiserror::Error;

/// Result of a copy-on-write share operation.
pub type ShareFsResult<T> = Result<T, Report<ShareFsError>>;

/// Failure while opening or mutating a copy-on-write share.
#[derive(Debug, Error)]
pub enum ShareFsError {
    /// The configured lower directory is not an absolute, real directory.
    #[error("invalid share lower directory {path}")]
    InvalidLowerDirectory {
        /// Invalid lower directory.
        path: PathBuf,
    },
    /// The configured state path is not an absolute directory location.
    #[error("invalid share state directory {path}")]
    InvalidStateDirectory {
        /// Invalid state directory.
        path: PathBuf,
    },
    /// The lower and state directories overlap.
    #[error("share lower and state directories overlap")]
    OverlappingDirectories,
    /// Another process already owns the private upper state.
    #[error("share upper state is already in use")]
    StateInUse,
    /// A caller supplied an unsafe relative share path.
    #[error("invalid relative share path {path:?}")]
    InvalidPath {
        /// Invalid path.
        path: PathBuf,
    },
    /// The requested entry does not exist in the merged filesystem.
    #[error("share entry does not exist: {path:?}")]
    NotFound {
        /// Missing merged path.
        path: PathBuf,
    },
    /// The requested destination already exists in the merged filesystem.
    #[error("share entry already exists: {path:?}")]
    AlreadyExists {
        /// Existing merged path.
        path: PathBuf,
    },
    /// An operation required a directory.
    #[error("share entry is not a directory: {path:?}")]
    NotDirectory {
        /// Non-directory merged path.
        path: PathBuf,
    },
    /// An operation required a non-directory entry.
    #[error("share entry is a directory: {path:?}")]
    IsDirectory {
        /// Directory merged path.
        path: PathBuf,
    },
    /// A directory could not be removed or replaced because it is not empty.
    #[error("share directory is not empty: {path:?}")]
    DirectoryNotEmpty {
        /// Non-empty merged directory.
        path: PathBuf,
    },
    /// The lower filesystem entry uses an unsupported type.
    #[error("unsupported share entry type: {path:?}")]
    UnsupportedEntryType {
        /// Unsupported merged path.
        path: PathBuf,
    },
    /// A lower-backed merged directory cannot yet be renamed.
    #[error("cannot rename a dynamically merged lower directory: {path:?}")]
    LowerDirectoryRename {
        /// Lower-backed directory.
        path: PathBuf,
    },
    /// The lower entry changed while it was being captured.
    #[error("share lower entry changed while being captured: {path:?}")]
    ConcurrentLowerChange {
        /// Unstable lower path.
        path: PathBuf,
    },
    /// Durable upper state is malformed or incomplete.
    #[error("invalid durable share state")]
    CorruptState,
    /// A filesystem operation failed.
    #[error("failed to {action}")]
    Io {
        /// Operation that failed.
        action: &'static str,
        /// Underlying filesystem error.
        #[source]
        source: io::Error,
    },
    /// A namespace-index operation failed.
    #[error("failed to {action}")]
    Database {
        /// Operation that failed.
        action: &'static str,
        /// Underlying `SQLite` error.
        #[source]
        source: rusqlite::Error,
    },
    /// A kernel FUSE session could not be mounted or stopped.
    #[error("failed to {action}")]
    Fuse {
        /// Operation that failed.
        action: &'static str,
        /// Underlying FUSE session error.
        #[source]
        source: io::Error,
    },
    /// The in-memory filesystem state lock was poisoned.
    #[error("share filesystem state lock was poisoned")]
    Poisoned,
}
