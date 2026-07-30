//! Extensions for generated chat values.

use thiserror::Error;

use crate::ArcStr;
use crate::types::chats::ChatCostCenterId;

/// Maximum byte length of a workspace-local chat cost-center identifier.
pub const MAX_CHAT_COST_CENTER_ID_BYTES: usize = 64;

/// Returns whether text satisfies the portable chat cost-center ID contract.
#[must_use]
pub fn is_valid_chat_cost_center_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_CHAT_COST_CENTER_ID_BYTES
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric() || character == '-' || character == '_'
        })
}

/// Failure to parse a workspace-local chat cost-center identifier.
#[derive(Debug, Error)]
#[error(
    "invalid chat cost-center identifier: expected 1-64 ASCII letters, digits, hyphens, or underscores"
)]
pub struct ChatCostCenterIdParseError;

impl ChatCostCenterId {
    /// Creates a workspace-local cost-center identifier from validated text.
    #[must_use]
    pub fn new(value: impl Into<ArcStr>) -> Self {
        Self(value.into())
    }

    /// Returns the cost-center identifier as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        self.0.as_ref()
    }
}

impl std::fmt::Display for ChatCostCenterId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl std::str::FromStr for ChatCostCenterId {
    type Err = ChatCostCenterIdParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if !is_valid_chat_cost_center_id(value) {
            return Err(ChatCostCenterIdParseError);
        }
        Ok(Self::new(value))
    }
}
