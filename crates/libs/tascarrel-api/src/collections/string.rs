//! Copy-on-write string used by generated API types.

use std::borrow::Borrow;
use std::fmt;
use std::ops::Deref;
use std::ops::DerefMut;

use ecow::EcoString;

/// An efficiently cloneable, copy-on-write string.
///
/// This is a transparent smart-pointer-style wrapper around [`EcoString`].
#[derive(Clone, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(transparent)]
pub struct ArcStr(EcoString);

impl ArcStr {
    /// Creates an empty string.
    #[must_use]
    pub const fn new() -> Self {
        Self(EcoString::new())
    }

    /// Creates an empty string with space for at least `capacity` bytes.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self(EcoString::with_capacity(capacity))
    }

    /// Appends a string slice.
    pub fn push_str(&mut self, value: &str) {
        self.0.push_str(value);
    }

    /// Appends a character.
    pub fn push(&mut self, value: char) {
        self.0.push(value);
    }
}

impl fmt::Debug for ArcStr {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl fmt::Display for ArcStr {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl Deref for ArcStr {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.0.as_str()
    }
}

impl DerefMut for ArcStr {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.0.make_mut()
    }
}

impl AsRef<str> for ArcStr {
    fn as_ref(&self) -> &str {
        self
    }
}

impl Borrow<str> for ArcStr {
    fn borrow(&self) -> &str {
        self
    }
}

impl From<&str> for ArcStr {
    fn from(value: &str) -> Self {
        Self(value.into())
    }
}

impl From<String> for ArcStr {
    fn from(value: String) -> Self {
        Self(value.into())
    }
}

impl From<EcoString> for ArcStr {
    fn from(value: EcoString) -> Self {
        Self(value)
    }
}

impl From<ArcStr> for EcoString {
    fn from(value: ArcStr) -> Self {
        value.0
    }
}

impl From<ArcStr> for String {
    fn from(value: ArcStr) -> Self {
        value.0.into()
    }
}

impl PartialEq<str> for ArcStr {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

impl PartialEq<&str> for ArcStr {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

impl PartialEq<ArcStr> for str {
    fn eq(&self, other: &ArcStr) -> bool {
        self == other.0
    }
}

impl PartialEq<ArcStr> for &str {
    fn eq(&self, other: &ArcStr) -> bool {
        *self == other.0
    }
}

impl serde::Serialize for ArcStr {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.0.serialize(serializer)
    }
}

impl<'de> serde::Deserialize<'de> for ArcStr {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        EcoString::deserialize(deserializer).map(Self)
    }
}
