//! Extensions for generated repository inventory values.

use crate::ArcStr;
use crate::types::repositories::RepositoryRevision;

impl RepositoryRevision {
    /// Creates a revision from its lowercase SHA-256 representation.
    #[must_use]
    pub fn new(value: impl Into<ArcStr>) -> Self {
        Self(value.into())
    }

    /// Returns the revision as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        self.0.as_ref()
    }
}

impl std::fmt::Display for RepositoryRevision {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}
