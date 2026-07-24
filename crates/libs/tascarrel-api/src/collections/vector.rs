//! Copy-on-write Sidex sequence adapter backed by `ecow`.

use std::fmt;
use std::hash::Hash;
use std::ops::Deref;
use std::ops::DerefMut;

use ecow::EcoVec;
use serde::ser::SerializeSeq as _;

/// An efficiently cloneable, copy-on-write vector.
///
/// This is a transparent wrapper around [`EcoVec`] that supplies the generic
/// serialization adapters required by Sidex-generated types.
#[repr(transparent)]
pub struct ArcVec<T>(EcoVec<T>);

impl<T> ArcVec<T> {
    /// Creates an empty sequence.
    #[must_use]
    pub const fn new() -> Self {
        Self(EcoVec::new())
    }

    /// Creates an empty sequence with space for at least `capacity` elements.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self(EcoVec::with_capacity(capacity))
    }

    /// Removes all elements.
    pub fn clear(&mut self) {
        self.0.clear();
    }
}

impl<T: Clone> ArcVec<T> {
    /// Appends an element, cloning the allocation only when it is shared.
    pub fn push(&mut self, value: T) {
        self.0.push(value);
    }

    /// Removes and returns the element at `index`.
    pub fn remove(&mut self, index: usize) -> T {
        self.0.remove(index)
    }
}

impl<T> Default for ArcVec<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Clone> Clone for ArcVec<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<T: fmt::Debug> fmt::Debug for ArcVec<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl<T: PartialEq> PartialEq for ArcVec<T> {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl<T: Eq> Eq for ArcVec<T> {}

impl<T: PartialOrd> PartialOrd for ArcVec<T> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        self.0.partial_cmp(&other.0)
    }
}

impl<T: Ord> Ord for ArcVec<T> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.cmp(&other.0)
    }
}

impl<T: Hash> Hash for ArcVec<T> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.0.hash(state);
    }
}

impl<T> Deref for ArcVec<T> {
    type Target = [T];

    fn deref(&self) -> &Self::Target {
        self.0.as_slice()
    }
}

impl<T: Clone> DerefMut for ArcVec<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.0.make_mut()
    }
}

impl<T> AsRef<[T]> for ArcVec<T> {
    fn as_ref(&self) -> &[T] {
        self
    }
}

impl<T> From<EcoVec<T>> for ArcVec<T> {
    fn from(value: EcoVec<T>) -> Self {
        Self(value)
    }
}

impl<T> From<ArcVec<T>> for EcoVec<T> {
    fn from(value: ArcVec<T>) -> Self {
        value.0
    }
}

impl<T: Clone> From<Vec<T>> for ArcVec<T> {
    fn from(value: Vec<T>) -> Self {
        Self(value.into())
    }
}

impl<T: Clone> FromIterator<T> for ArcVec<T> {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        Self(iter.into_iter().collect())
    }
}

impl<'a, T> IntoIterator for &'a ArcVec<T> {
    type Item = &'a T;
    type IntoIter = std::slice::Iter<'a, T>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl<T: Clone> IntoIterator for ArcVec<T> {
    type Item = T;
    type IntoIter = ecow::vec::IntoIter<T>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

impl<T> serde::Serialize for ArcVec<T>
where
    T: serde::Serialize,
{
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.0.serialize(serializer)
    }
}

impl<'de, T> serde::Deserialize<'de> for ArcVec<T>
where
    T: serde::Deserialize<'de> + Clone,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        EcoVec::deserialize(deserializer).map(Self)
    }
}

impl<T, U> sidex_serde::SerializeAs<ArcVec<T>> for ArcVec<U>
where
    U: sidex_serde::SerializeAs<T>,
{
    fn serialize_as<S>(value: &ArcVec<T>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut sequence = serializer.serialize_seq(Some(value.len()))?;
        for value in value {
            sequence.serialize_element(&sidex_serde::SerializeAsWrap::<T, U>::new(value))?;
        }
        sequence.end()
    }
}

impl<'de, T, U> sidex_serde::DeserializeAs<'de, ArcVec<T>> for ArcVec<U>
where
    T: Clone,
    U: sidex_serde::DeserializeAs<'de, T>,
{
    fn deserialize_as<D>(deserializer: D) -> Result<ArcVec<T>, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        <Vec<sidex_serde::DeserializeAsWrap<T, U>> as serde::Deserialize>::deserialize(deserializer)
            .map(|values| {
                values
                    .into_iter()
                    .map(sidex_serde::DeserializeAsWrap::into_inner)
                    .collect()
            })
    }
}

impl<T> sidex_serde::SidexType for ArcVec<T>
where
    T: sidex_serde::SidexType + Clone,
{
    type Encoding = ArcVec<T::Encoding>;
}
