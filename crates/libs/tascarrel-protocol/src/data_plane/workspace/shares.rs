//! Bounded manifest for host directories attached to one workspace VM.
//!
//! The host resolves and pins directories before starting QEMU. The guest only
//! receives opaque mount tags and safe names, so later workspace configuration
//! changes cannot redirect a live VM's devices.

use std::collections::HashSet;

use reportify::ErrorExt as _;
use reportify::Report;
use serde::Deserialize;
use serde::Serialize;
use thiserror::Error;

/// Host-pinned shares attached to the current workspace VM.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceHostSharesResponse {
    /// Shares in deterministic VM-device order.
    pub shares: Vec<WorkspaceHostShare>,
}

impl WorkspaceHostSharesResponse {
    /// Validates the manifest's count, names, tags, and uniqueness.
    ///
    /// # Errors
    ///
    /// Returns a report when a peer sends a manifest outside protocol bounds.
    pub fn validate(&self) -> Result<(), Report<WorkspaceHostSharesMessageError>> {
        if self.shares.len() > MAX_WORKSPACE_HOST_SHARES {
            return Err(invalid_message(
                "workspace host-share manifest has too many entries",
            ));
        }
        let mut names = HashSet::with_capacity(self.shares.len());
        let mut mount_tags = HashSet::with_capacity(self.shares.len());
        for share in &self.shares {
            if !valid_workspace_share_name(&share.name) {
                return Err(invalid_message(
                    "workspace host-share manifest contains an invalid name",
                ));
            }
            if !valid_mount_tag(&share.mount_tag) {
                return Err(invalid_message(
                    "workspace host-share manifest contains an invalid mount tag",
                ));
            }
            if !names.insert(share.name.as_str()) || !mount_tags.insert(share.mount_tag.as_str()) {
                return Err(invalid_message(
                    "workspace host-share manifest contains duplicate entries",
                ));
            }
        }
        Ok(())
    }
}

/// One host directory attached to the workspace VM.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceHostShare {
    /// Validated name used below `/mnt/shares` and `/mnt`.
    pub name: String,
    /// Opaque virtiofs or virtio-9p mount tag assigned by the host.
    pub mount_tag: String,
    /// Whether the guest and its pods may modify the export.
    pub writable: bool,
}

/// Invalid or oversized workspace host-share response.
#[derive(Debug, Error)]
#[error("invalid workspace host-share protocol message: {message}")]
pub struct WorkspaceHostSharesMessageError {
    message: &'static str,
}

/// Maximum encoded response size for the private workspace-shares channel.
pub const MAX_WORKSPACE_SHARES_FRAME_LEN: usize = 16 * 1024;
/// Maximum number of host shares attached to one workspace VM.
pub const MAX_WORKSPACE_HOST_SHARES: usize = 32;
/// Maximum UTF-8 byte length of one host-share name.
pub const MAX_WORKSPACE_SHARE_NAME_BYTES: usize = 64;
/// Maximum UTF-8 byte length of one guest mount tag.
pub const MAX_WORKSPACE_SHARE_MOUNT_TAG_BYTES: usize = 64;

/// Checks the portable grammar used for host-share names.
#[must_use]
pub fn valid_workspace_share_name(name: &str) -> bool {
    if name.is_empty() || name.len() > MAX_WORKSPACE_SHARE_NAME_BYTES {
        return false;
    }
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphanumeric())
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn valid_mount_tag(tag: &str) -> bool {
    !tag.is_empty()
        && tag.len() <= MAX_WORKSPACE_SHARE_MOUNT_TAG_BYTES
        && tag
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn invalid_message(message: &'static str) -> Report<WorkspaceHostSharesMessageError> {
    WorkspaceHostSharesMessageError { message }.report()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verifies portable names and unique mount tags form a valid manifest.
    #[test]
    fn accepts_bounded_unique_manifest() {
        WorkspaceHostSharesResponse {
            shares: vec![
                WorkspaceHostShare {
                    name: "source".to_owned(),
                    mount_tag: "tascarrel-share-0".to_owned(),
                    writable: false,
                },
                WorkspaceHostShare {
                    name: "build_cache".to_owned(),
                    mount_tag: "tascarrel-share-1".to_owned(),
                    writable: true,
                },
            ],
        }
        .validate()
        .unwrap();
    }

    /// Verifies names cannot escape the guest's fixed mount directories.
    #[test]
    fn rejects_unsafe_share_names() {
        for name in ["", ".hidden", "../source", "source/cache", "source cache"] {
            assert!(!valid_workspace_share_name(name), "{name:?}");
        }
    }

    /// Verifies both names and device tags must remain unique.
    #[test]
    fn rejects_duplicate_names_or_mount_tags() {
        for duplicate_tag in [false, true] {
            let response = WorkspaceHostSharesResponse {
                shares: vec![
                    WorkspaceHostShare {
                        name: "source".to_owned(),
                        mount_tag: "tascarrel-share-0".to_owned(),
                        writable: false,
                    },
                    WorkspaceHostShare {
                        name: if duplicate_tag { "cache" } else { "source" }.to_owned(),
                        mount_tag: if duplicate_tag {
                            "tascarrel-share-0"
                        } else {
                            "tascarrel-share-1"
                        }
                        .to_owned(),
                        writable: false,
                    },
                ],
            };
            assert!(response.validate().is_err());
        }
    }
}
