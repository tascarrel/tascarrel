//! Extensions for generated workspace values.

use crate::ArcStr;
use crate::types::workspaces::UsbDeviceId;
use crate::types::workspaces::WorkspaceName;

impl WorkspaceName {
    /// Creates a workspace name from its string representation.
    #[must_use]
    pub fn new(value: impl Into<ArcStr>) -> Self {
        Self(value.into())
    }

    /// Returns the workspace name as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        self.0.as_ref()
    }
}

impl std::fmt::Display for WorkspaceName {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl UsbDeviceId {
    /// Creates a connection-scoped USB device identifier.
    #[must_use]
    pub fn new(value: impl Into<ArcStr>) -> Self {
        Self(value.into())
    }

    /// Returns the identifier as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        self.0.as_ref()
    }
}

impl std::fmt::Display for UsbDeviceId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}
