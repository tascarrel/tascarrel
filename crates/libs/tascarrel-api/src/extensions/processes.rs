//! Extensions for generated process values and terminal payloads.

use std::sync::Arc;

use base64::Engine as _;

/// Arbitrary terminal bytes encoded as standard padded Base64 in JSON.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ProcessTerminalData(Arc<[u8]>);

impl ProcessTerminalData {
    /// Returns the terminal bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// Returns whether the value contains no bytes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Returns the number of bytes in the value.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }
}

impl AsRef<[u8]> for ProcessTerminalData {
    fn as_ref(&self) -> &[u8] {
        self.as_bytes()
    }
}

impl From<Vec<u8>> for ProcessTerminalData {
    fn from(value: Vec<u8>) -> Self {
        Self(value.into())
    }
}

impl From<Arc<[u8]>> for ProcessTerminalData {
    fn from(value: Arc<[u8]>) -> Self {
        Self(value)
    }
}

impl From<&[u8]> for ProcessTerminalData {
    fn from(value: &[u8]) -> Self {
        Self(value.into())
    }
}

impl std::str::FromStr for ProcessTerminalData {
    type Err = ProcessTerminalDataDecodeError;

    fn from_str(encoded: &str) -> Result<Self, Self::Err> {
        base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map(Self::from)
            .map_err(|_| ProcessTerminalDataDecodeError)
    }
}

impl serde::Serialize for ProcessTerminalData {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&base64::engine::general_purpose::STANDARD.encode(&self.0))
    }
}

impl<'de> serde::Deserialize<'de> for ProcessTerminalData {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let encoded = <String as serde::Deserialize>::deserialize(deserializer)?;
        encoded.parse().map_err(serde::de::Error::custom)
    }
}

/// A JSON process-terminal-data string was not valid standard padded Base64.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProcessTerminalDataDecodeError;

impl std::fmt::Display for ProcessTerminalDataDecodeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("process terminal data must be standard padded Base64")
    }
}

impl std::error::Error for ProcessTerminalDataDecodeError {}

#[cfg(test)]
mod tests {
    use super::ProcessTerminalData;

    /// Verifies the API representation preserves arbitrary terminal bytes in
    /// standard padded Base64 form.
    #[test]
    fn serializes_terminal_bytes_as_base64() {
        let data = ProcessTerminalData::from(&b"\0\xffterminal\r\n"[..]);
        let encoded = serde_json::to_string(&data).unwrap();
        assert_eq!(encoded, r#""AP90ZXJtaW5hbA0K""#);
        assert_eq!(
            serde_json::from_str::<ProcessTerminalData>(&encoded).unwrap(),
            data
        );
    }

    /// Verifies malformed Base64 cannot enter the typed terminal byte stream.
    #[test]
    fn rejects_non_base64_json_strings() {
        assert!(serde_json::from_str::<ProcessTerminalData>(r#""not base64!""#).is_err());
    }
}
