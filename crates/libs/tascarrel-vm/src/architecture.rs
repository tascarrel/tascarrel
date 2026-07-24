//! Supported guest architectures and host architecture detection.
//!
//! [`Architecture`] identifies a QEMU target and [`Architecture::host`]
//! detects the current host target.

use std::fmt;
use std::str::FromStr;

use reportify::Report;
use thiserror::Error;

/// A guest instruction-set architecture supported by Tascarrel.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Architecture {
    /// AMD64/x86-64.
    X86_64,
    /// 64-bit ARM.
    Aarch64,
}

impl Architecture {
    /// Detects the architecture of the process running this library.
    ///
    /// Rust architecture aliases such as `amd64` and `arm64` are accepted by
    /// [`FromStr`], while the compile-time values normally produced by Rust are
    /// `x86_64` and `aarch64`.
    ///
    /// # Errors
    ///
    /// Returns [`ArchitectureParseError`] if Rust reports an architecture other
    /// than x86-64 or `AArch64`.
    pub fn host() -> Result<Self, Report<ArchitectureParseError>> {
        Self::from_str(std::env::consts::ARCH)
    }

    /// Returns the conventional QEMU system executable name.
    #[must_use]
    pub const fn qemu_binary(self) -> &'static str {
        match self {
            Self::X86_64 => "qemu-system-x86_64",
            Self::Aarch64 => "qemu-system-aarch64",
        }
    }

    /// Returns the QEMU machine model used by Tascarrel.
    #[must_use]
    pub const fn qemu_machine(self) -> &'static str {
        match self {
            Self::X86_64 => "q35",
            Self::Aarch64 => "virt",
        }
    }
}

impl fmt::Display for Architecture {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::X86_64 => "x86_64",
            Self::Aarch64 => "aarch64",
        })
    }
}

impl FromStr for Architecture {
    type Err = Report<ArchitectureParseError>;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.to_ascii_lowercase().as_str() {
            "x86_64" | "x86-64" | "amd64" => Ok(Self::X86_64),
            "aarch64" | "arm64" => Ok(Self::Aarch64),
            _ => Err(Report::new(ArchitectureParseError(value.to_owned()))),
        }
    }
}

/// Error returned when an architecture name is unsupported.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("unsupported architecture `{0}` (supported: x86_64, aarch64)")]
pub struct ArchitectureParseError(pub(crate) String);

impl ArchitectureParseError {
    /// Returns the architecture name that could not be parsed.
    #[must_use]
    pub fn input(&self) -> &str {
        &self.0
    }
}
