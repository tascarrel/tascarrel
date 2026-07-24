//! Bounded host-resolved workspace environment protocol.
//!
//! The authenticated workspace mux supplies identity. The host sends exactly
//! one framed response, and the guest validates aggregate environment limits
//! before exposing values to pod processes.

use std::collections::BTreeMap;

use reportify::ErrorExt as _;
use reportify::Report;
use serde::Deserialize;
use serde::Serialize;
use thiserror::Error;

/// Maximum encoded response size for the private workspace environment channel.
pub const MAX_WORKSPACE_ENVIRONMENT_FRAME_LEN: usize = 128 * 1024;
/// Maximum resolved workspace environment entries.
pub const MAX_WORKSPACE_ENVIRONMENT_ENTRIES: usize = 128;
/// Maximum aggregate bytes across resolved workspace environment names and
/// values.
pub const MAX_WORKSPACE_ENVIRONMENT_BYTES: usize = 64 * 1024;
/// Maximum guest-safe failure diagnostic length.
pub const MAX_WORKSPACE_ENVIRONMENT_FAILURE_BYTES: usize = 2 * 1024;

/// Host result containing resolved environment values or a safe failure
/// diagnostic.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceEnvironmentResponse {
    /// Resolved environment or a failure that contains no secret material.
    pub result: Result<BTreeMap<String, String>, WorkspaceEnvironmentFailure>,
}

impl WorkspaceEnvironmentResponse {
    /// Validates entry count, aggregate size, environment names, and NUL
    /// exclusion.
    ///
    /// # Errors
    ///
    /// Returns a report when a peer sends an environment outside protocol
    /// bounds.
    pub fn validate(&self) -> Result<(), Report<WorkspaceEnvironmentMessageError>> {
        match &self.result {
            Ok(environment) => validate_environment(environment),
            Err(failure)
                if failure.message.len() > MAX_WORKSPACE_ENVIRONMENT_FAILURE_BYTES
                    || failure.message.contains('\0') =>
            {
                Err(invalid_message(
                    "workspace environment failure is outside protocol bounds",
                ))
            }
            Err(_) => Ok(()),
        }
    }
}

/// Validates a successful workspace environment response.
fn validate_environment(
    environment: &BTreeMap<String, String>,
) -> Result<(), Report<WorkspaceEnvironmentMessageError>> {
    if environment.len() > MAX_WORKSPACE_ENVIRONMENT_ENTRIES {
        return Err(invalid_message(
            "workspace environment has too many entries",
        ));
    }
    let mut bytes = 0_usize;
    for (name, value) in environment {
        if !valid_environment_name(name) || value.contains('\0') {
            return Err(invalid_message(
                "workspace environment contains an invalid entry",
            ));
        }
        bytes = bytes
            .checked_add(name.len())
            .and_then(|size| size.checked_add(value.len()))
            .ok_or_else(|| invalid_message("workspace environment size overflowed"))?;
    }
    if bytes > MAX_WORKSPACE_ENVIRONMENT_BYTES {
        return Err(invalid_message("workspace environment is too large"));
    }
    Ok(())
}

/// Safe host failure returned when workspace environment resolution fails.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceEnvironmentFailure {
    /// Human-readable diagnostic containing no secret material.
    pub message: String,
}

/// Invalid or oversized workspace environment response.
#[derive(Debug, Error)]
#[error("invalid workspace environment protocol message: {message}")]
pub struct WorkspaceEnvironmentMessageError {
    message: &'static str,
}

/// Checks the portable process-environment name grammar.
fn valid_environment_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|byte| byte == b'_' || byte.is_ascii_alphabetic())
        && bytes.all(|byte| byte == b'_' || byte.is_ascii_alphanumeric())
}

/// Creates a protocol validation report with a static safe diagnostic.
fn invalid_message(message: &'static str) -> Report<WorkspaceEnvironmentMessageError> {
    WorkspaceEnvironmentMessageError { message }.report()
}
