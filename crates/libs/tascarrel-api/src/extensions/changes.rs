//! Extensions for generated Git inspection values.
//!
//! The generated nominal string types expose matching constructors and string
//! views through this module.

use crate::ArcStr;
use crate::types::changes::GitFileMode;
use crate::types::changes::GitObjectId;
use crate::types::changes::GitReference;
use crate::types::changes::RepositoryPath;
use crate::types::changes::UnifiedDiff;

macro_rules! string_value {
    ($type:ty, $description:literal) => {
        impl $type {
            #[doc = concat!("Creates ", $description, " from a validated string.")]
            #[must_use]
            pub fn new(value: impl Into<ArcStr>) -> Self {
                Self(value.into())
            }

            #[doc = concat!("Returns ", $description, " as a string slice.")]
            #[must_use]
            pub fn as_str(&self) -> &str {
                self.0.as_ref()
            }
        }

        impl std::fmt::Display for $type {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str(self.as_str())
            }
        }
    };
}

string_value!(GitObjectId, "a Git object identifier");
string_value!(GitReference, "a Git reference");
string_value!(RepositoryPath, "a repository-relative path");
string_value!(GitFileMode, "a Git file mode");
string_value!(UnifiedDiff, "a unified diff");
