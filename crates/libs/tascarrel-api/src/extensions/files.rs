//! Extensions for generated workspace file paths.
//!
//! [`FilePath::new`] is the common constructor used after service-side path
//! validation, and [`MAX_RELATIVE_PATH_BYTES`] defines the shared size bound.

use crate::ArcStr;
use crate::types::files::FilePath;

impl FilePath {
    /// Creates a path from a value validated by a filesystem service.
    #[must_use]
    pub fn new(value: impl Into<ArcStr>) -> Self {
        Self(value.into())
    }

    /// Returns the workspace-relative path as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        self.0.as_ref()
    }
}

impl std::fmt::Display for FilePath {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Maximum encoded byte length of a workspace- or repository-relative path.
pub const MAX_RELATIVE_PATH_BYTES: usize = 4_096;
