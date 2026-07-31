//! Extensions for overlay-share revision values.

use crate::ArcStr;
use crate::types::shares::ShareOverlayApprovalListRevision;
use crate::types::shares::ShareOverlayRevision;

impl ShareOverlayApprovalListRevision {
    /// Creates a list revision from its lowercase SHA-256 representation.
    #[must_use]
    pub fn new(value: impl Into<ArcStr>) -> Self {
        Self(value.into())
    }

    /// Returns the list revision as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        self.0.as_ref()
    }
}

impl std::fmt::Display for ShareOverlayApprovalListRevision {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl ShareOverlayRevision {
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

impl std::fmt::Display for ShareOverlayRevision {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}
