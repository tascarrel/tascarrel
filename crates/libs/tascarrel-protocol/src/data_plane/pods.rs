//! Workspace and pod identities, metadata, and health values.

use std::fmt;
use std::str::FromStr;

use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use serde::de;

/// A path-safe workspace identifier supplied by a local host client.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct WorkspaceName(String);

impl WorkspaceName {
    /// Parses and validates a workspace name.
    ///
    /// # Errors
    ///
    /// Returns [`WorkspaceNameError`] unless `name` consists of 1-64 ASCII
    /// letters, digits, `_`, or `-`.
    pub fn new(name: impl Into<String>) -> Result<Self, WorkspaceNameError> {
        let name = name.into();
        if name.is_empty()
            || name.len() > 64
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        {
            return Err(WorkspaceNameError);
        }
        Ok(Self(name))
    }

    /// Returns the validated workspace name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consumes the validated name.
    #[must_use]
    pub fn into_inner(self) -> String {
        self.0
    }
}

impl fmt::Display for WorkspaceName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl AsRef<str> for WorkspaceName {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl FromStr for WorkspaceName {
    type Err = WorkspaceNameError;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        Self::new(name)
    }
}

impl TryFrom<String> for WorkspaceName {
    type Error = WorkspaceNameError;

    fn try_from(name: String) -> Result<Self, Self::Error> {
        Self::new(name)
    }
}

impl TryFrom<&str> for WorkspaceName {
    type Error = WorkspaceNameError;

    fn try_from(name: &str) -> Result<Self, Self::Error> {
        Self::new(name)
    }
}

impl<'de> Deserialize<'de> for WorkspaceName {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .try_into()
            .map_err(de::Error::custom)
    }
}

/// A workspace name failed validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("workspace names must contain 1-64 ASCII letters, digits, '_', or '-'")]
pub struct WorkspaceNameError;

/// Identifies a pod within a guest VM.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PodId(pub String);

impl fmt::Display for PodId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Coarse health level shared by workspace VMs and pods.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthStatus {
    #[default]
    Healthy,
    Degraded,
    Error,
}

/// Current health plus bounded human-readable diagnostics.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Health {
    pub status: HealthStatus,
    #[serde(default)]
    pub messages: Vec<String>,
}

impl Health {
    #[must_use]
    pub const fn healthy() -> Self {
        Self {
            status: HealthStatus::Healthy,
            messages: Vec::new(),
        }
    }

    #[must_use]
    pub fn degraded(message: impl Into<String>) -> Self {
        Self {
            status: HealthStatus::Degraded,
            messages: vec![bounded_health_message(message.into())],
        }
    }

    #[must_use]
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            status: HealthStatus::Error,
            messages: vec![bounded_health_message(message.into())],
        }
    }
}

fn bounded_health_message(mut message: String) -> String {
    const MAX_BYTES: usize = 4096;
    if message.len() <= MAX_BYTES {
        return message;
    }
    let mut end = MAX_BYTES - '…'.len_utf8();
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    message.truncate(end);
    message.push('…');
    message
}

/// Metadata about a guest pod.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pod {
    pub id: PodId,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub user: String,
    /// Guest-side outer UID mapped to the image-selected user inside the pod.
    pub uid: u32,
    /// Guest-side outer GID mapped to the image-selected user's primary group.
    pub gid: u32,
    pub created_at_unix_ms: u64,
    #[serde(default)]
    pub health: Health,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pod_titles_are_optional_for_durable_protocol_compatibility() {
        let pod: Pod = serde_json::from_value(serde_json::json!({
            "id": "pod-1",
            "name": "quiet-otter",
            "user": "develop",
            "uid": 1000,
            "gid": 1000,
            "created_at_unix_ms": 1
        }))
        .unwrap();
        assert_eq!(pod.title, None);

        let titled = Pod {
            title: Some("Repair workspace startup logs".to_owned()),
            ..pod
        };
        assert_eq!(
            serde_json::to_value(titled).unwrap()["title"],
            "Repair workspace startup logs"
        );
    }

    #[test]
    fn workspace_names_are_validated_at_every_boundary() {
        for valid in ["default", "rust-1", "team-alpha", "A_B"] {
            let name = WorkspaceName::new(valid).unwrap();
            assert_eq!(name.as_str(), valid);
            assert_eq!(valid.parse::<WorkspaceName>().unwrap(), name);
            assert_eq!(
                serde_json::from_str::<WorkspaceName>(&format!("\"{valid}\"")).unwrap(),
                name
            );
        }

        for invalid in [
            "",
            ".",
            "..",
            "team.alpha",
            "../escape",
            "has space",
            "non-ascii-é",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        ] {
            assert!(WorkspaceName::new(invalid).is_err(), "accepted {invalid:?}");
            assert!(
                serde_json::from_str::<WorkspaceName>(&format!("\"{invalid}\"")).is_err(),
                "deserialized {invalid:?}"
            );
        }
    }

    #[test]
    fn health_diagnostics_are_bounded_on_utf8_boundaries() {
        let health = Health::degraded("ø".repeat(4096));
        assert!(health.messages[0].len() <= 4096);
        assert!(health.messages[0].ends_with('…'));
    }
}
