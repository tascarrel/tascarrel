//! Errors safe to expose across a data-plane channel.

use std::fmt;

use serde::Deserialize;
use serde::Serialize;

/// Stable, machine-readable categories for errors returned by a peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    InvalidRequest,
    NotFound,
    AlreadyExists,
    PermissionDenied,
    Busy,
    ResourceExhausted,
    Unsupported,
    ExecutionFailed,
    Internal,
}

/// A typed error safe to send to the remote peer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteError {
    pub code: ErrorCode,
    pub message: String,
}

impl RemoteError {
    #[must_use]
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl fmt::Display for RemoteError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} ({:?})", self.message, self.code)
    }
}

impl std::error::Error for RemoteError {}
