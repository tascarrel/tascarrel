//! Validated identifiers for persistent images and pod runtime state.

use std::fmt;
use std::str::FromStr;

use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use serde::Serializer;
use serde::de;
use thiserror::Error;

const MAX_POD_ID_LEN: usize = 64;
const MAX_DIGEST_ALGORITHM_LEN: usize = 32;
const MIN_DIGEST_ENCODED_LEN: usize = 16;
const MAX_DIGEST_ENCODED_LEN: usize = 128;

/// A rejected image or pod identifier.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("invalid {kind}: {reason}")]
pub struct IdentifierError {
    kind: &'static str,
    reason: &'static str,
}

impl IdentifierError {
    const fn pod(reason: &'static str) -> Self {
        Self {
            kind: "pod ID",
            reason,
        }
    }

    const fn image(reason: &'static str) -> Self {
        Self {
            kind: "image digest",
            reason,
        }
    }
}

/// A path-safe Tascarrel pod identifier.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PodId(String);

impl PodId {
    /// Validates and constructs a pod identifier.
    ///
    /// IDs contain at most 64 ASCII letters, digits, `_`, or `-`, and must
    /// begin with an alphanumeric character. This deliberately excludes path
    /// separators and the special `.` and `..` components.
    ///
    /// # Errors
    ///
    /// Returns an error when `value` is empty, too long, or contains a byte
    /// outside the accepted path-safe alphabet.
    pub fn new(value: impl Into<String>) -> Result<Self, IdentifierError> {
        let value = value.into();
        if value.is_empty() || value.len() > MAX_POD_ID_LEN {
            return Err(IdentifierError::pod(
                "length must be between 1 and 64 bytes",
            ));
        }
        let mut bytes = value.bytes();
        if !bytes
            .next()
            .is_some_and(|byte| byte.is_ascii_alphanumeric())
        {
            return Err(IdentifierError::pod(
                "the first byte must be an ASCII letter or digit",
            ));
        }
        if !bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')) {
            return Err(IdentifierError::pod(
                "only ASCII letters, digits, `_`, and `-` are allowed",
            ));
        }
        Ok(Self(value))
    }

    /// Returns the validated identifier.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PodId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl FromStr for PodId {
    type Err = IdentifierError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl Serialize for PodId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for PodId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(de::Error::custom)
    }
}

/// A validated OCI-style content digest used as an image generation ID.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ImageId(String);

impl ImageId {
    /// Validates and constructs an image digest.
    ///
    /// The accepted form is `algorithm:encoded`. The algorithm follows OCI's
    /// lowercase token form and the encoded portion is restricted to a safe
    /// ASCII token. Consequently the complete digest is also a single safe
    /// Linux path component.
    ///
    /// # Errors
    ///
    /// Returns an error when the digest is malformed, too long, too short, or
    /// contains a path-unsafe byte.
    pub fn new(value: impl Into<String>) -> Result<Self, IdentifierError> {
        let value = value.into();
        let Some((algorithm, encoded)) = value.split_once(':') else {
            return Err(IdentifierError::image("expected `algorithm:encoded`"));
        };
        if algorithm.is_empty() || algorithm.len() > MAX_DIGEST_ALGORITHM_LEN {
            return Err(IdentifierError::image(
                "algorithm length must be between 1 and 32 bytes",
            ));
        }
        let mut previous_separator = false;
        for (index, byte) in algorithm.bytes().enumerate() {
            let separator = matches!(byte, b'+' | b'.' | b'_' | b'-');
            if !(byte.is_ascii_lowercase() || byte.is_ascii_digit() || separator)
                || (separator && index == 0)
                || (separator && previous_separator)
            {
                return Err(IdentifierError::image(
                    "algorithm contains an invalid or repeated separator",
                ));
            }
            previous_separator = separator;
        }
        if previous_separator {
            return Err(IdentifierError::image(
                "algorithm must not end with a separator",
            ));
        }
        if !(MIN_DIGEST_ENCODED_LEN..=MAX_DIGEST_ENCODED_LEN).contains(&encoded.len()) {
            return Err(IdentifierError::image(
                "encoded digest length must be between 16 and 128 bytes",
            ));
        }
        if !encoded
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'='))
        {
            return Err(IdentifierError::image(
                "encoded digest contains a path-unsafe byte",
            ));
        }
        Ok(Self(value))
    }

    /// Returns the canonical digest string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ImageId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl FromStr for ImageId {
    type Err = IdentifierError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl Serialize for ImageId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ImageId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verifies pod identifiers are safe single path components.
    #[test]
    fn pod_ids_are_single_safe_components() {
        for valid in ["p", "pod-123", "Pod_ABC", "9"] {
            assert_eq!(PodId::new(valid).unwrap().as_str(), valid);
        }
        for invalid in ["", ".", "..", "-pod", "pod/name", "pod:name", "pod name"] {
            assert!(PodId::new(invalid).is_err(), "accepted {invalid:?}");
        }
        assert!(PodId::new("x".repeat(65)).is_err());
    }

    /// Verifies image identifiers accept OCI tokens but reject paths.
    #[test]
    fn image_ids_accept_oci_tokens_but_not_paths() {
        let digest = format!("sha256:{}", "a".repeat(64));
        assert_eq!(ImageId::new(&digest).unwrap().as_str(), digest);
        for invalid in [
            "sha256",
            "SHA256:0123456789abcdef",
            "sha256:short",
            "sha256:0123456789abcde/",
            ".sha256:0123456789abcdef",
            "../sha256:0123456789abcdef",
            "sha256::0123456789abcdef",
        ] {
            assert!(ImageId::new(invalid).is_err(), "accepted {invalid:?}");
        }
    }

    /// Verifies deserialization applies identifier validation.
    #[test]
    fn deserialization_revalidates_identifiers() {
        assert!(serde_json::from_str::<PodId>(r#""../pod""#).is_err());
        assert!(serde_json::from_str::<ImageId>(r#""sha256:../bad""#).is_err());
    }
}
