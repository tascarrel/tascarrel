//! Parsing helpers for workspace configuration values.

use reportify::ErrorExt as _;
use reportify::Report;
use thiserror::Error;

/// Error returned when a binary size is malformed or too large.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub enum BinarySizeError {
    /// The value is not a positive integer with a supported binary suffix.
    #[error("invalid binary size: expected a positive integer followed by M, G, or T")]
    InvalidFormat,
    /// The value cannot be represented in the requested output unit.
    #[error("binary size exceeds the supported range")]
    OutOfRange,
}

/// Parses a binary size into bytes.
///
/// `M`, `G`, and `T` are binary units; the explicit `MiB`, `GiB`, and `TiB`
/// spellings are accepted as well. Suffix matching is case-insensitive.
///
/// # Errors
///
/// Returns [`BinarySizeError`] for fractional, zero, unsuffixed, unknown, or
/// unrepresentably large values.
pub fn parse_size_bytes(value: &str) -> Result<u64, Report<BinarySizeError>> {
    let (amount, multiplier_mib) = parse_size_parts(value)?;
    amount
        .checked_mul(multiplier_mib)
        .and_then(|mib| mib.checked_mul(1024 * 1024))
        .filter(|bytes| *bytes > 0)
        .ok_or_else(|| BinarySizeError::OutOfRange.report())
}

/// Parses a binary memory size into MiB.
///
/// `M`, `G`, and `T` are binary units; the explicit `MiB`, `GiB`, and `TiB`
/// spellings are accepted as well. Suffix matching is case-insensitive.
///
/// # Errors
///
/// Returns [`BinarySizeError`] for fractional, zero, unsuffixed, unknown, or
/// unrepresentably large values.
pub fn parse_memory_mib(value: &str) -> Result<u32, Report<BinarySizeError>> {
    let (amount, multiplier) = parse_size_parts(value)?;
    let memory_mib = amount
        .checked_mul(multiplier)
        .filter(|memory| *memory > 0)
        .ok_or_else(|| BinarySizeError::OutOfRange.report())?;
    u32::try_from(memory_mib).map_err(|_| BinarySizeError::OutOfRange.report())
}

fn parse_size_parts(value: &str) -> Result<(u64, u64), Report<BinarySizeError>> {
    let uppercase = value.to_ascii_uppercase();
    let (number, multiplier) = [
        ("TIB", 1024_u64 * 1024),
        ("GIB", 1024_u64),
        ("MIB", 1_u64),
        ("T", 1024_u64 * 1024),
        ("G", 1024_u64),
        ("M", 1_u64),
    ]
    .into_iter()
    .find_map(|(suffix, multiplier)| {
        uppercase
            .strip_suffix(suffix)
            .map(|number| (number, multiplier))
    })
    .ok_or_else(|| BinarySizeError::InvalidFormat.report())?;
    if number.is_empty() || !number.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(BinarySizeError::InvalidFormat.report());
    }
    let amount = number
        .parse::<u64>()
        .map_err(|_| BinarySizeError::OutOfRange.report())?;
    Ok((amount, multiplier))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verifies binary units and rejection of ambiguous size values.
    #[test]
    fn parses_binary_units_and_rejects_ambiguous_values() {
        assert_eq!(parse_memory_mib("16G").unwrap(), 16 * 1024);
        assert_eq!(parse_memory_mib("1536MiB").unwrap(), 1536);
        assert_eq!(parse_memory_mib("2tib").unwrap(), 2 * 1024 * 1024);
        assert_eq!(parse_size_bytes("1T").unwrap(), 1024_u64.pow(4));
        for invalid in ["", "16", "1.5G", "-1G", "0M", "16GB"] {
            assert!(parse_memory_mib(invalid).is_err(), "accepted {invalid:?}");
        }
    }
}
