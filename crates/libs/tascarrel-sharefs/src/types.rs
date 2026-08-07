//! Public merged-entry and change-set values.
//!
//! [`ShareChange`] describes one approval candidate, and [`LowerLease`]
//! retains the lower identity used to detect concurrent host changes.
//! [`FileWriteOutcome`] reports revision-checked editor replacements.

use std::ffi::OsString;
use std::fmt;
use std::path::PathBuf;

/// One canonical path change retained in the private upper state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShareChange {
    /// Relative path affected by the change.
    pub path: PathBuf,
    /// Lower lease captured when the path was first overridden.
    pub base: Option<LowerLease>,
    /// Proposed upper version, or absence for a deletion.
    pub proposed: Option<EntryVersion>,
    /// Whether the proposed directory hides every lower child.
    pub opaque: bool,
}

/// Lower entry captured when a merged path was first overridden.
///
/// The identity and timestamp fields provide a cheap optimistic comparison.
/// A mismatch is only a conflict candidate: regular files and symbolic links
/// can still be hashed and compared with [`Self::version`] before approval.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LowerLease {
    /// Content and logical metadata captured from the lower entry.
    pub version: EntryVersion,
    /// Lower modification time captured with the version.
    pub modified_at: FileTime,
    /// Lower inode-change time captured with the version.
    pub changed_at: FileTime,
    /// Device containing the captured lower inode.
    pub device: u64,
    /// Captured lower inode number.
    pub inode: u64,
}

/// Content and metadata used as one side of an approval comparison.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EntryVersion {
    /// Entry type.
    pub kind: EntryKind,
    /// File length or symbolic-link target length.
    pub size: u64,
    /// Logical Unix permission bits.
    pub mode: u32,
    /// Content or symbolic-link digest when applicable.
    pub content_digest: Option<ContentDigest>,
}

/// One entry returned from a merged directory enumeration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DirectoryEntry {
    /// Entry name relative to the enumerated directory.
    pub name: OsString,
    /// Current merged metadata.
    pub metadata: EntryMetadata,
}

/// Metadata exposed for one merged entry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EntryMetadata {
    /// Entry type.
    pub kind: EntryKind,
    /// File length or symbolic-link target length.
    pub size: u64,
    /// Logical Unix permission bits.
    pub mode: u32,
    /// Last content-modification time.
    pub modified_at: FileTime,
}

/// Outcome of one revision-checked complete-file replacement.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileWriteOutcome {
    /// The file was replaced with the supplied contents.
    Written {
        /// Digest of the replacement contents.
        revision: ContentDigest,
    },
    /// The current contents do not match the expected revision.
    Conflict {
        /// Digest of the current contents.
        revision: ContentDigest,
    },
}

/// Supported merged filesystem entry type.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EntryKind {
    /// Regular file.
    File,
    /// Directory.
    Directory,
    /// Symbolic link.
    Symlink,
}

/// Filesystem timestamp represented without platform-dependent types.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct FileTime {
    /// Whole seconds since the Unix epoch.
    pub seconds: i64,
    /// Nanoseconds within the second.
    pub nanoseconds: u32,
}

/// SHA-256 digest of file contents or a symbolic-link target.
#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub struct ContentDigest(pub(crate) [u8; 32]);

impl ContentDigest {
    /// Computes the digest of one byte string.
    #[must_use]
    pub fn from_bytes(bytes: &[u8]) -> Self {
        use sha2::Digest as _;

        Self(sha2::Sha256::digest(bytes).into())
    }

    /// Creates a digest from its raw bytes.
    #[must_use]
    pub const fn from_array(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Returns the digest bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Debug for ContentDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, formatter)
    }
}

impl fmt::Display for ContentDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}
