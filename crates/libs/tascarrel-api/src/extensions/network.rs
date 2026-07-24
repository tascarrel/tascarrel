//! Extensions for generated network values.

use reportify::ErrorExt as _;
use reportify::Report;
use thiserror::Error;

use crate::ArcStr;
use crate::types::network::HostnamePrefix;
use crate::types::network::PortMapping;

impl HostnamePrefix {
    /// Creates a hostname prefix from a value validated by hostd.
    #[must_use]
    pub fn new(value: impl Into<ArcStr>) -> Self {
        Self(value.into())
    }

    /// Returns the hostname prefix as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        self.0.as_ref()
    }
}

impl std::fmt::Display for HostnamePrefix {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Failure to parse a same-port shorthand or `<host-port>:<pod-port>` mapping.
#[derive(Debug, Error)]
#[error(
    "invalid port mapping: expected <port> or <host-port>:<pod-port> with ports from 1 through 65535"
)]
pub struct PortMappingError;

impl PortMapping {
    /// Parses and constructs a validated Docker-style TCP port mapping.
    ///
    /// # Errors
    ///
    /// Returns an error unless the input contains nonzero decimal `u16` ports,
    /// either as one same-port shorthand or separated by one colon.
    pub fn parse(value: impl AsRef<str>) -> Result<Self, Report<PortMappingError>> {
        let value = value.as_ref();
        parse_ports(value)?;
        Ok(Self(ArcStr::from(value)))
    }

    /// Returns the host and pod ports represented by this mapping.
    ///
    /// # Errors
    ///
    /// Returns an error if this value entered through deserialization without
    /// satisfying the mapping contract.
    pub fn ports(&self) -> Result<(u16, u16), Report<PortMappingError>> {
        parse_ports(self.as_str())
    }

    /// Returns the mapping as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        self.0.as_ref()
    }
}

impl std::fmt::Display for PortMapping {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Parses the two ports while enforcing the canonical mapping grammar.
fn parse_ports(value: &str) -> Result<(u16, u16), Report<PortMappingError>> {
    let Some((host, pod)) = value.split_once(':') else {
        if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(PortMappingError.report());
        }
        let port = value
            .parse::<u16>()
            .map_err(|_| PortMappingError.report())?;
        if port == 0 {
            return Err(PortMappingError.report());
        }
        return Ok((port, port));
    };
    if host.is_empty()
        || pod.is_empty()
        || pod.contains(':')
        || !host.bytes().all(|byte| byte.is_ascii_digit())
        || !pod.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(PortMappingError.report());
    }
    let host = host.parse::<u16>().map_err(|_| PortMappingError.report())?;
    let pod = pod.parse::<u16>().map_err(|_| PortMappingError.report())?;
    if host == 0 || pod == 0 {
        return Err(PortMappingError.report());
    }
    Ok((host, pod))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verifies canonical mappings expose host and pod ports in Docker order.
    #[test]
    fn mapping_parses_host_then_pod_port() {
        let mapping = PortMapping::parse("5432:15432").unwrap();
        assert_eq!(mapping.ports().unwrap(), (5432, 15432));
        assert_eq!(mapping.as_str(), "5432:15432");
        assert_eq!(
            PortMapping::parse("3000").unwrap().ports().unwrap(),
            (3000, 3000)
        );
    }

    /// Verifies ambiguous, noncanonical, zero, and out-of-range ports fail.
    #[test]
    fn mapping_rejects_invalid_port_pairs() {
        for mapping in [
            "", "5432:", ":15432", "0", "0:1", "1:0", "1:2:3", " 1:2", "65536", "65536:1",
        ] {
            assert!(PortMapping::parse(mapping).is_err(), "accepted {mapping:?}");
        }
    }
}
