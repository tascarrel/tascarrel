//! Copy-on-write filesystem mechanics for host shares exposed to Tascarrel
//! pods.
//!
//! [`ShareFileSystem`] merges a live, read-only lower directory with private
//! per-pod changes. Untouched names are always resolved from the current lower
//! directory. Created and modified nodes shadow lower entries, while durable
//! whiteouts hide deleted entries. Merged directories remain live unless a pod
//! explicitly deletes and recreates a directory, which makes the replacement
//! opaque.
//!
//! The private state directory contains a transactional `SQLite` namespace
//! index and regular files addressed by internal node identifiers.
//! [`MountedShareFileSystem`] exposes the merged view through the kernel FUSE
//! interface. Authorization and application of approved changes remain daemon
//! responsibilities.

#![deny(unsafe_code)]

mod error;
mod filesystem;
mod fuse;
mod state;
mod types;

pub use error::ShareFsError;
pub use error::ShareFsResult;
pub use filesystem::FrozenShareFileSystem;
pub use filesystem::ShareFileSystem;
pub use fuse::MountedShareFileSystem;
pub use fuse::ShareFileSystemMountOptions;
pub use types::ContentDigest;
pub use types::DirectoryEntry;
pub use types::EntryKind;
pub use types::EntryMetadata;
pub use types::EntryVersion;
pub use types::FileTime;
pub use types::LowerLease;
pub use types::ShareChange;
