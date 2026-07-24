//! Bounded private network protocol between one workspace guest and hostd.
//!
//! DNS channels carry one semantic query followed by one bounded DNS response
//! message. TCP channels carry one attributed open request and response before
//! switching to an unframed byte stream. Workspace identity is supplied by the
//! authenticated multiplex connection and is never accepted from these
//! messages.

use std::net::Ipv4Addr;
use std::net::SocketAddr;

use reportify::ErrorExt as _;
use reportify::Report;
use serde::Deserialize;
use serde::Serialize;
use thiserror::Error;

use crate::PodId;

/// Guest-to-host endpoint for one semantic DNS request.
pub const MUX_NETWORK_DNS_ENDPOINT: &str = "tascarrel-network-dns-v1";
/// Guest-to-host endpoint for one attributed TCP flow.
pub const MUX_NETWORK_TCP_ENDPOINT: &str = "tascarrel-network-tcp-v1";
/// Guest-only address at which guestd terminates workload DNS.
pub const VIRTUAL_DNS_ADDRESS: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 53);
/// Guest-only address for explicitly exposed host-loopback services.
pub const VIRTUAL_HOST_ADDRESS: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 54);
/// Maximum framed private-network request or response size.
pub const MAX_NETWORK_FRAME_LEN: usize = 256 * 1024;
/// Maximum encoded DNS response accepted from hostd.
pub const MAX_DNS_MESSAGE_LEN: usize = 65_535;

const MAX_DNS_NAME_LEN: usize = 255;

/// Invalid or oversized private-network protocol message.
#[derive(Debug, Error)]
#[error("invalid private-network protocol message: {message}")]
pub struct NetworkMessageError {
    message: &'static str,
}

/// Trusted guest-derived workload attribution.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum NetworkSource {
    /// A process in one workspace pod.
    Pod(PodId),
    /// The isolated workspace image-build environment.
    ImageBuild,
    /// A trusted workspace guest service outside a pod or image build.
    WorkspaceService,
}

/// Transport used between a workload and guestd's DNS resolver.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DnsClientTransport {
    /// A DNS UDP datagram terminated inside the guest.
    Udp,
    /// A length-delimited DNS TCP message terminated inside the guest.
    Tcp,
}

/// One semantic DNS question issued by a guest workload.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DnsResolveRequest {
    /// Trusted workload attribution assigned by guestd.
    pub source: NetworkSource,
    /// Workload-to-guest transport used for the query.
    pub transport: DnsClientTransport,
    /// Canonical absolute DNS name.
    pub name: String,
    /// Numeric DNS resource-record type.
    pub record_type: u16,
    /// Numeric DNS class.
    pub record_class: u16,
    /// Whether the workload requested recursive resolution.
    pub recursion_desired: bool,
    /// Whether the workload advertised DNSSEC support.
    pub dnssec_ok: bool,
}

impl DnsResolveRequest {
    /// Checks bounds required before host-side resolution.
    ///
    /// # Errors
    ///
    /// Returns a report when the question is empty, oversized, or relative.
    pub fn validate(&self) -> Result<(), Report<NetworkMessageError>> {
        if self.name.is_empty() || self.name.len() > MAX_DNS_NAME_LEN {
            return Err(invalid_message("DNS name has an invalid length"));
        }
        if !self.name.ends_with('.') {
            return Err(invalid_message("DNS name is not absolute"));
        }
        if self.record_class == 0 || self.record_type == 0 {
            return Err(invalid_message("DNS class and record type must be nonzero"));
        }
        Ok(())
    }
}

/// Host result for one semantic DNS request.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DnsResolveResponse {
    /// Encoded DNS response produced from the semantic resolver result.
    pub result: Result<Vec<u8>, NetworkFailure>,
}

impl DnsResolveResponse {
    /// Checks response size and failure-message bounds.
    ///
    /// # Errors
    ///
    /// Returns a report when the response exceeds protocol bounds.
    pub fn validate(&self) -> Result<(), Report<NetworkMessageError>> {
        match &self.result {
            Ok(message) if message.len() > MAX_DNS_MESSAGE_LEN => {
                Err(invalid_message("DNS response exceeds the protocol limit"))
            }
            Err(error) => error.validate(),
            _ => Ok(()),
        }
    }
}

/// First framed message on one guest-originated TCP channel.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TcpFlowOpenRequest {
    /// Trusted workload attribution assigned by guestd.
    pub source: NetworkSource,
    /// Source address observed by guestd's transparent listener.
    pub source_address: SocketAddr,
    /// Original destination recovered by guestd.
    pub destination: SocketAddr,
}

impl TcpFlowOpenRequest {
    /// Checks socket-address invariants before host admission.
    ///
    /// # Errors
    ///
    /// Returns a report when either socket uses port zero.
    pub fn validate(&self) -> Result<(), Report<NetworkMessageError>> {
        if self.source_address.port() == 0 || self.destination.port() == 0 {
            return Err(invalid_message(
                "TCP source and destination ports must be nonzero",
            ));
        }
        Ok(())
    }
}

/// Successful result of opening a host-side TCP socket or proxy.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TcpFlowConnected {
    /// Local address assigned to a raw host-side socket, when available.
    pub local_address: Option<SocketAddr>,
}

/// Host response before a TCP channel switches to raw stream bytes.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TcpFlowOpenResponse {
    /// Connected flow details or a bounded terminal failure.
    pub result: Result<TcpFlowConnected, NetworkFailure>,
}

impl TcpFlowOpenResponse {
    /// Checks peer-safe failure bounds.
    ///
    /// # Errors
    ///
    /// Returns a report when a failure message is empty or oversized.
    pub fn validate(&self) -> Result<(), Report<NetworkMessageError>> {
        self.result
            .as_ref()
            .map_or_else(NetworkFailure::validate, |_| Ok(()))
    }
}

/// Stable private-network failure category.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkFailureCode {
    /// The guest request violated the private protocol contract.
    InvalidRequest,
    /// Workspace network policy denied the operation.
    Denied,
    /// A configured concurrency or queue limit was reached.
    Overloaded,
    /// Resolution or connection establishment exceeded its deadline.
    TimedOut,
    /// The required resolver or destination was unavailable.
    Unavailable,
    /// Resolution or connection establishment failed.
    Failed,
}

/// Bounded failure safe to return to a guest peer.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkFailure {
    /// Stable failure category.
    pub code: NetworkFailureCode,
    /// Display-safe diagnostic containing at most 2048 bytes.
    pub message: String,
}

impl NetworkFailure {
    /// Creates a bounded private-network failure.
    #[must_use]
    pub fn new(code: NetworkFailureCode, message: impl Into<String>) -> Self {
        let mut message = message.into();
        if message.len() > 2048 {
            let boundary = message
                .char_indices()
                .map(|(index, _)| index)
                .take_while(|index| *index <= 2048)
                .last()
                .unwrap_or(0);
            message.truncate(boundary);
        }
        Self { code, message }
    }

    fn validate(&self) -> Result<(), Report<NetworkMessageError>> {
        if self.message.is_empty() || self.message.len() > 2048 {
            Err(invalid_message(
                "network failure message has an invalid length",
            ))
        } else {
            Ok(())
        }
    }
}

fn invalid_message(message: &'static str) -> Report<NetworkMessageError> {
    NetworkMessageError { message }.report()
}
