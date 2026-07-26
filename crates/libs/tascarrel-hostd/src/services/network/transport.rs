//! Guest-to-host DNS resolution and TCP flow transport.

use std::io;
use std::net::IpAddr;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use hickory_proto::op::Message;
use hickory_proto::op::MessageType;
use hickory_proto::op::OpCode;
use hickory_proto::rr::DNSClass;
use hickory_proto::rr::Name;
use hickory_proto::rr::RData;
use hickory_proto::rr::RecordType;
use hickory_resolver::net::DnsError;
use hickory_resolver::net::NetError;
use reportify::ErrorExt as _;
use reportify::Report;
use reportify::ResultExt as _;
use tascarrel_api::types::network as api;
use tascarrel_api::types::workspaces::WorkspaceName;
use tascarrel_mux::Channel;
use tascarrel_protocol::Framed;
use tascarrel_protocol::network::DnsClientTransport;
use tascarrel_protocol::network::DnsResolveRequest;
use tascarrel_protocol::network::DnsResolveResponse;
use tascarrel_protocol::network::MAX_NETWORK_FRAME_LEN;
use tascarrel_protocol::network::NetworkFailure;
use tascarrel_protocol::network::NetworkFailureCode;
use tascarrel_protocol::network::NetworkSource;
use tascarrel_protocol::network::TcpFlowConnected;
use tascarrel_protocol::network::TcpFlowOpenRequest;
use tascarrel_protocol::network::TcpFlowOpenResponse;
use tascarrel_protocol::network::VIRTUAL_HOST_ADDRESS;
use thiserror::Error;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::copy_bidirectional;
use tokio::net::TcpStream;
use tokio::time::timeout;

use super::NetworkPolicy;
use super::NetworkService;
use super::policy::forbidden_address;
use super::policy::host_interface_addresses;
use super::proxy::HttpProxy;
use crate::WorkspaceAuthority;
use crate::services::secrets::SecretsService;

/// Failure while serving one private guest network channel.
#[derive(Debug, Error)]
pub(crate) enum NetworkTransportError {
    #[error("network protocol failed: {0}")]
    Protocol(String),
    #[error("network transport failed: {0}")]
    Transport(#[from] io::Error),
    #[error("HTTP network proxy failed")]
    Proxy,
}

#[derive(Clone, Copy, Debug)]
enum ProxyMode {
    Http,
    Https,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum TcpDestination {
    Socket(SocketAddr),
    ConfiguredHost { host: String, port: u16 },
}

impl TcpDestination {
    const fn port(&self) -> u16 {
        match self {
            Self::Socket(address) => address.port(),
            Self::ConfiguredHost { port, .. } => *port,
        }
    }

    const fn effective_address(&self) -> Option<SocketAddr> {
        match self {
            Self::Socket(address) => Some(*address),
            Self::ConfiguredHost { .. } => None,
        }
    }
}

impl NetworkService {
    /// Resolves one semantic DNS request carried by a private workspace
    /// channel.
    #[tracing::instrument(
        name = "tascarrel_host.network.resolve_dns",
        level = "debug",
        skip(self, channel),
        fields(workspace = %workspace.as_str()),
        err(Debug)
    )]
    pub(crate) async fn serve_dns_channel(
        &self,
        workspace: &WorkspaceName,
        channel: Channel,
    ) -> Result<(), Report<NetworkTransportError>> {
        let started_at = Instant::now();
        let occurred_at = jiff::Timestamp::now();
        let mut framed = Framed::with_max_frame_len(channel, MAX_NETWORK_FRAME_LEN)
            .map_err(|error| NetworkTransportError::Protocol(error.to_string()).report())?;
        let Some(request) = timeout(
            self.inner.config.dns_timeout,
            framed.read::<DnsResolveRequest>(),
        )
        .await
        .map_err(|_| NetworkTransportError::Protocol("DNS request timed out".to_owned()).report())?
        .map_err(|error| NetworkTransportError::Protocol(error.to_string()).report())?
        else {
            return Err(NetworkTransportError::Protocol(
                "DNS channel closed before its request".to_owned(),
            )
            .report());
        };
        let mut trailing = [0_u8; 1];
        match timeout(
            self.inner.config.dns_timeout,
            framed.get_mut().read(&mut trailing),
        )
        .await
        {
            Ok(Ok(0)) => {}
            Ok(Ok(_)) => {
                return Err(NetworkTransportError::Protocol(
                    "DNS channel carried data after its request".to_owned(),
                )
                .report());
            }
            Ok(Err(error)) => return Err(NetworkTransportError::Transport(error).report()),
            Err(_) => {
                return Err(NetworkTransportError::Protocol(
                    "DNS request stream did not close".to_owned(),
                )
                .report());
            }
        }
        let source = network_source(&request.source)?;
        let resolution = match request.validate() {
            Ok(()) => self.resolve_dns(&request).await,
            Err(error) => Err(DnsResolutionError::Invalid(error.to_string())),
        };
        let (response, outcome, resolved_addresses) = match resolution {
            Ok(message) => {
                let resolved_addresses = dns_addresses(&message);
                let summary = dns_summary(
                    &message,
                    &resolved_addresses,
                    self.inner.config.dns_address_summary_limit.get(),
                );
                let bytes = message
                    .to_vec()
                    .map_err(|error| NetworkTransportError::Protocol(error.to_string()).report())?;
                (
                    DnsResolveResponse { result: Ok(bytes) },
                    api::DnsRequestOutcome::Response(summary),
                    resolved_addresses,
                )
            }
            Err(error) => {
                let (code, outcome) = error.protocol_result();
                (
                    DnsResolveResponse {
                        result: Err(NetworkFailure::new(code, error.to_string())),
                    },
                    outcome,
                    Vec::new(),
                )
            }
        };
        self.record_dns_request(
            workspace,
            api::DnsRequest {
                occurred_at,
                source,
                transport: match request.transport {
                    DnsClientTransport::Udp => api::DnsRequestTransport::Udp,
                    DnsClientTransport::Tcp => api::DnsRequestTransport::Tcp,
                },
                name: request.name.into(),
                record_type: request.record_type,
                record_class: request.record_class,
                outcome,
                duration_ms: duration_ms(started_at.elapsed()),
            },
            &resolved_addresses,
        );
        response
            .validate()
            .map_err(|error| NetworkTransportError::Protocol(error.to_string()).report())?;
        framed
            .write(&response)
            .await
            .map_err(|error| NetworkTransportError::Protocol(error.to_string()).report())?;
        let mut channel = framed.into_inner();
        channel
            .shutdown()
            .await
            .map_err(|error| NetworkTransportError::Transport(error).report())?;
        Ok(())
    }

    /// Opens and relays one attributed TCP flow carried by a private workspace
    /// channel.
    #[tracing::instrument(
        name = "tascarrel_host.network.relay_tcp",
        level = "debug",
        skip(self, policy, authority, secrets, channel),
        fields(workspace = %workspace.as_str()),
        err(Debug)
    )]
    #[allow(
        clippy::too_many_lines,
        reason = "the relay keeps one TCP flow's paired start and terminal events together"
    )]
    pub(crate) async fn serve_tcp_channel(
        &self,
        workspace: &WorkspaceName,
        policy: &NetworkPolicy,
        authority: Option<Arc<WorkspaceAuthority>>,
        secrets: &SecretsService,
        channel: Channel,
    ) -> Result<(), Report<NetworkTransportError>> {
        let started_at = Instant::now();
        let mut framed = Framed::with_max_frame_len(channel, MAX_NETWORK_FRAME_LEN)
            .map_err(|error| NetworkTransportError::Protocol(error.to_string()).report())?;
        let Some(request) = timeout(
            self.inner.config.connect_timeout,
            framed.read::<TcpFlowOpenRequest>(),
        )
        .await
        .map_err(|_| {
            NetworkTransportError::Protocol("TCP open request timed out".to_owned()).report()
        })?
        .map_err(|error| NetworkTransportError::Protocol(error.to_string()).report())?
        else {
            return Err(NetworkTransportError::Protocol(
                "TCP channel closed before its open request".to_owned(),
            )
            .report());
        };
        let source = network_source(&request.source)?;
        let pod_host_forward = (request.destination.ip() == IpAddr::V4(VIRTUAL_HOST_ADDRESS))
            .then(|| {
                self.pod_host_forward_destination(workspace, &source, request.destination.port())
            })
            .flatten();
        let tcp_flow_id = api::TcpFlowId::generate();
        let admission = request
            .validate()
            .map_err(|error| TcpAdmissionError::Invalid(error.to_string()))
            .and_then(|()| {
                tcp_destination(
                    request.destination,
                    policy,
                    pod_host_forward,
                    &self.inner.config.host_port_host,
                )
            });
        let (destination, proxy) = match admission {
            Ok(admission) => admission,
            Err(error) => {
                self.record_tcp_start(workspace, &tcp_flow_id, &request, source, None, None);
                self.write_tcp_failure(&mut framed, workspace, tcp_flow_id, started_at, error)
                    .await?;
                return Ok(());
            }
        };
        let mode = match proxy {
            None => api::TcpFlowMode::Raw,
            Some(ProxyMode::Http) => api::TcpFlowMode::Http,
            Some(ProxyMode::Https) => api::TcpFlowMode::Https,
        };
        self.record_tcp_start(
            workspace,
            &tcp_flow_id,
            &request,
            source,
            destination.effective_address(),
            Some(mode),
        );
        let Ok(permit) = Arc::clone(&self.inner.connections).try_acquire_owned() else {
            self.write_tcp_failure(
                &mut framed,
                workspace,
                tcp_flow_id,
                started_at,
                TcpAdmissionError::Overloaded,
            )
            .await?;
            return Ok(());
        };
        let result: Result<(), Report<NetworkTransportError>> = if let Some(proxy) = proxy {
            match framed
                .write(&TcpFlowOpenResponse {
                    result: Ok(TcpFlowConnected {
                        local_address: None,
                    }),
                })
                .await
            {
                Ok(()) => {
                    let proxy_service = HttpProxy::new(
                        policy.clone(),
                        authority,
                        self.inner.config.connect_timeout,
                    );
                    let channel = framed.into_inner();
                    match proxy {
                        ProxyMode::Http => {
                            proxy_service
                                .serve_http(
                                    channel,
                                    destination.port(),
                                    workspace.clone(),
                                    secrets.clone(),
                                )
                                .await
                        }
                        ProxyMode::Https => {
                            proxy_service
                                .serve_https(
                                    channel,
                                    destination.port(),
                                    workspace.clone(),
                                    secrets.clone(),
                                )
                                .await
                        }
                    }
                    .escalate(NetworkTransportError::Proxy)
                }
                Err(error) => Err(NetworkTransportError::Protocol(error.to_string()).report()),
            }
        } else {
            let mut upstream = match timeout(
                self.inner.config.connect_timeout,
                connect_tcp_destination(&destination),
            )
            .await
            {
                Ok(Ok(upstream)) => upstream,
                Ok(Err(error)) => {
                    self.write_tcp_failure(
                        &mut framed,
                        workspace,
                        tcp_flow_id,
                        started_at,
                        TcpAdmissionError::Failed(error.to_string()),
                    )
                    .await?;
                    return Ok(());
                }
                Err(_) => {
                    self.write_tcp_failure(
                        &mut framed,
                        workspace,
                        tcp_flow_id,
                        started_at,
                        TcpAdmissionError::TimedOut,
                    )
                    .await?;
                    return Ok(());
                }
            };
            let local_address = match upstream.local_addr() {
                Ok(address) => Some(address),
                Err(error) => {
                    tracing::debug!(%error, "failed to inspect TCP upstream local address");
                    None
                }
            };
            match framed
                .write(&TcpFlowOpenResponse {
                    result: Ok(TcpFlowConnected { local_address }),
                })
                .await
            {
                Ok(()) => {
                    let mut channel = framed.into_inner();
                    copy_bidirectional(&mut channel, &mut upstream)
                        .await
                        .map(|_| ())
                        .map_err(|error| NetworkTransportError::Transport(error).report())
                }
                Err(error) => Err(NetworkTransportError::Protocol(error.to_string()).report()),
            }
        };
        drop(permit);
        let outcome = if result.is_ok() {
            api::TcpFlowOutcome::Closed
        } else {
            api::TcpFlowOutcome::Failed
        };
        self.record_tcp_end(workspace, tcp_flow_id, started_at, outcome);
        result
    }

    async fn resolve_dns(
        &self,
        request: &DnsResolveRequest,
    ) -> Result<Message, DnsResolutionError> {
        if DNSClass::from(request.record_class) != DNSClass::IN {
            return Err(DnsResolutionError::Invalid(
                "only the Internet DNS class is supported".to_owned(),
            ));
        }
        let name = request
            .name
            .parse::<Name>()
            .map_err(|error| DnsResolutionError::Invalid(error.to_string()))?;
        let record_type = RecordType::from(request.record_type);
        let lookup = timeout(
            self.inner.config.dns_timeout,
            self.inner.resolver.lookup(name.clone(), record_type),
        )
        .await
        .map_err(|_| DnsResolutionError::TimedOut)?;
        match lookup {
            Ok(lookup) => {
                let mut message = lookup.message().clone();
                message.metadata.id = 0;
                message.metadata.message_type = MessageType::Response;
                message.metadata.recursion_desired = request.recursion_desired;
                message.metadata.recursion_available = true;
                Ok(message)
            }
            Err(NetError::Dns(DnsError::NoRecordsFound(no_records))) => {
                let mut message = Message::response(0, OpCode::Query);
                message.add_query((*no_records.query).clone());
                message.metadata.response_code = no_records.response_code;
                message.metadata.recursion_desired = request.recursion_desired;
                message.metadata.recursion_available = true;
                if let Some(soa) = no_records.soa {
                    message.add_authority((*soa).into_record_of_rdata());
                }
                if let Some(authorities) = no_records.authorities {
                    message.add_authorities(authorities.iter().cloned());
                }
                Ok(message)
            }
            Err(NetError::Dns(DnsError::ResponseCode(code))) => {
                let mut message = Message::error_msg(0, OpCode::Query, code);
                let mut query = hickory_proto::op::Query::query(name, record_type);
                query.query_class = DNSClass::IN;
                message.add_query(query);
                message.metadata.recursion_desired = request.recursion_desired;
                message.metadata.recursion_available = true;
                Ok(message)
            }
            Err(error) => Err(DnsResolutionError::Failed(error.to_string())),
        }
    }

    fn record_tcp_start(
        &self,
        workspace: &WorkspaceName,
        tcp_flow_id: &api::TcpFlowId,
        request: &TcpFlowOpenRequest,
        source: api::NetworkRequestSource,
        effective_destination: Option<SocketAddr>,
        mode: Option<api::TcpFlowMode>,
    ) {
        self.record_tcp_flow(
            workspace,
            api::TcpFlowEvent::Started(api::TcpFlowStarted {
                tcp_flow_id: tcp_flow_id.clone(),
                occurred_at: jiff::Timestamp::now(),
                source,
                source_address: request.source_address.to_string().into(),
                requested_destination: request.destination.to_string().into(),
                effective_destination: effective_destination.map(|value| value.to_string().into()),
                hostname: self
                    .dns_hostname(workspace, request.destination.ip())
                    .map(Into::into),
                mode: mode.unwrap_or(api::TcpFlowMode::Raw),
            }),
        );
    }

    fn record_tcp_end(
        &self,
        workspace: &WorkspaceName,
        tcp_flow_id: api::TcpFlowId,
        started_at: Instant,
        outcome: api::TcpFlowOutcome,
    ) {
        self.record_tcp_flow(
            workspace,
            api::TcpFlowEvent::Ended(api::TcpFlowEnded {
                tcp_flow_id,
                occurred_at: jiff::Timestamp::now(),
                outcome,
                duration_ms: duration_ms(started_at.elapsed()),
            }),
        );
    }

    async fn write_tcp_failure(
        &self,
        framed: &mut Framed<Channel>,
        workspace: &WorkspaceName,
        tcp_flow_id: api::TcpFlowId,
        started_at: Instant,
        error: TcpAdmissionError,
    ) -> Result<(), Report<NetworkTransportError>> {
        let (code, outcome) = error.protocol_result();
        self.record_tcp_end(workspace, tcp_flow_id, started_at, outcome);
        framed
            .write(&TcpFlowOpenResponse {
                result: Err(NetworkFailure::new(code, error.to_string())),
            })
            .await
            .map_err(|error| NetworkTransportError::Protocol(error.to_string()).report())?;
        Ok(())
    }
}

#[derive(Debug, Error)]
enum DnsResolutionError {
    #[error("invalid DNS request: {0}")]
    Invalid(String),
    #[error("DNS resolution timed out")]
    TimedOut,
    #[error("DNS resolution failed: {0}")]
    Failed(String),
}

impl DnsResolutionError {
    fn protocol_result(&self) -> (NetworkFailureCode, api::DnsRequestOutcome) {
        match self {
            Self::Invalid(_) => (
                NetworkFailureCode::InvalidRequest,
                api::DnsRequestOutcome::Invalid,
            ),
            Self::TimedOut => (
                NetworkFailureCode::TimedOut,
                api::DnsRequestOutcome::TimedOut,
            ),
            Self::Failed(_) => (NetworkFailureCode::Failed, api::DnsRequestOutcome::Failed),
        }
    }
}

#[derive(Debug, Error)]
enum TcpAdmissionError {
    #[error("invalid TCP request: {0}")]
    Invalid(String),
    #[error("TCP destination is denied by workspace policy")]
    Denied,
    #[error("host TCP flow capacity is exhausted")]
    Overloaded,
    #[error("TCP connection timed out")]
    TimedOut,
    #[error("TCP destination is unavailable: {0}")]
    Unavailable(String),
    #[error("TCP connection failed: {0}")]
    Failed(String),
}

impl TcpAdmissionError {
    fn protocol_result(&self) -> (NetworkFailureCode, api::TcpFlowOutcome) {
        match self {
            Self::Invalid(_) => (
                NetworkFailureCode::InvalidRequest,
                api::TcpFlowOutcome::Failed,
            ),
            Self::Denied => (NetworkFailureCode::Denied, api::TcpFlowOutcome::Denied),
            Self::Overloaded => (
                NetworkFailureCode::Overloaded,
                api::TcpFlowOutcome::Overloaded,
            ),
            Self::TimedOut => (NetworkFailureCode::TimedOut, api::TcpFlowOutcome::TimedOut),
            Self::Unavailable(_) => (
                NetworkFailureCode::Unavailable,
                api::TcpFlowOutcome::Unavailable,
            ),
            Self::Failed(_) => (NetworkFailureCode::Failed, api::TcpFlowOutcome::Failed),
        }
    }
}

fn network_source(
    source: &NetworkSource,
) -> Result<api::NetworkRequestSource, Report<NetworkTransportError>> {
    match source {
        NetworkSource::Pod(id) => id.0.parse().map(api::NetworkRequestSource::Pod).map_err(
            |error: tascarrel_api::ids::ParseIdError| {
                NetworkTransportError::Protocol(format!("invalid pod attribution: {error}"))
                    .report()
            },
        ),
        NetworkSource::ImageBuild => Ok(api::NetworkRequestSource::ImageBuild),
        NetworkSource::WorkspaceService => Ok(api::NetworkRequestSource::WorkspaceService),
    }
}

fn tcp_destination(
    requested: SocketAddr,
    policy: &NetworkPolicy,
    pod_host_forward: Option<SocketAddr>,
    host_port_host: &str,
) -> Result<(TcpDestination, Option<ProxyMode>), TcpAdmissionError> {
    if requested.ip() == IpAddr::V4(VIRTUAL_HOST_ADDRESS) {
        if let Some(destination) = pod_host_forward {
            return Ok((TcpDestination::Socket(destination), None));
        }
        let mapping = policy
            .host_ports
            .iter()
            .find(|mapping| mapping.pod_port == requested.port())
            .ok_or(TcpAdmissionError::Denied)?;
        return Ok((
            TcpDestination::ConfiguredHost {
                host: host_port_host.to_owned(),
                port: mapping.host_port,
            },
            None,
        ));
    }
    if !policy.allow_ports.contains(&requested.port()) {
        return Err(TcpAdmissionError::Denied);
    }
    let has_http_policy = !policy.allow_hosts.is_empty()
        || !policy.deny_hosts.is_empty()
        || !policy.secret_injection.is_empty();
    let proxy = if has_http_policy && policy.http_ports.contains(&requested.port()) {
        Some(ProxyMode::Http)
    } else if policy.needs_authority() && policy.https_ports.contains(&requested.port()) {
        Some(ProxyMode::Https)
    } else {
        None
    };
    if proxy.is_some() {
        return Ok((TcpDestination::Socket(requested), proxy));
    }
    let host_addresses = host_interface_addresses()
        .map_err(|error| TcpAdmissionError::Unavailable(error.to_string()))?;
    let allowed = !policy.deny_addresses.contains(&requested.ip())
        && (!policy.default_deny || policy.allow_addresses.contains(&requested.ip()))
        && !forbidden_address(requested.ip(), policy.allow_local, &host_addresses);
    if !allowed {
        return Err(TcpAdmissionError::Denied);
    }
    Ok((TcpDestination::Socket(requested), None))
}

async fn connect_tcp_destination(destination: &TcpDestination) -> io::Result<TcpStream> {
    match destination {
        TcpDestination::Socket(address) => TcpStream::connect(address).await,
        TcpDestination::ConfiguredHost { host, port } => {
            TcpStream::connect((host.as_str(), *port)).await
        }
    }
}

fn dns_addresses(message: &Message) -> Vec<IpAddr> {
    message
        .answers
        .iter()
        .filter_map(|record| match &record.data {
            RData::A(value) => Some(IpAddr::V4(value.0)),
            RData::AAAA(value) => Some(IpAddr::V6(value.0)),
            _ => None,
        })
        .collect()
}

fn dns_summary(
    message: &Message,
    resolved_addresses: &[IpAddr],
    address_limit: usize,
) -> api::DnsResponseSummary {
    api::DnsResponseSummary {
        response_code: u16::from(message.metadata.response_code),
        answer_count: u16::try_from(message.answers.len()).unwrap_or(u16::MAX),
        resolved_addresses: resolved_addresses
            .iter()
            .take(address_limit)
            .map(|address| address.to_string().into())
            .collect::<Vec<_>>()
            .into(),
        addresses_truncated: resolved_addresses.len() > address_limit,
    }
}

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use super::*;
    use crate::services::network::policy::HostPortMapping;

    /// Verifies static mappings translate the pod-visible port to the
    /// configured host-loopback port while preserving the same-port shorthand.
    #[test]
    fn virtual_host_uses_static_host_to_pod_port_mapping() {
        let policy = NetworkPolicy {
            host_ports: vec![
                HostPortMapping {
                    host_port: 5432,
                    pod_port: 15432,
                },
                HostPortMapping {
                    host_port: 3000,
                    pod_port: 3000,
                },
            ],
            ..NetworkPolicy::default()
        };
        let requested = SocketAddr::new(IpAddr::V4(VIRTUAL_HOST_ADDRESS), 15432);
        let (destination, proxy) =
            tcp_destination(requested, &policy, None, "outer.internal").unwrap();
        assert_eq!(
            destination,
            TcpDestination::ConfiguredHost {
                host: "outer.internal".to_owned(),
                port: 5432,
            }
        );
        assert!(proxy.is_none());
        let shorthand = SocketAddr::new(IpAddr::V4(VIRTUAL_HOST_ADDRESS), 3000);
        assert_eq!(
            tcp_destination(shorthand, &policy, None, "outer.internal")
                .unwrap()
                .0,
            TcpDestination::ConfiguredHost {
                host: "outer.internal".to_owned(),
                port: 3000,
            }
        );
    }

    /// Verifies a dynamic pod-scoped mapping takes precedence over a static
    /// mapping for the same pod-visible port.
    #[test]
    fn dynamic_pod_host_mapping_overrides_static_mapping() {
        let policy = NetworkPolicy {
            host_ports: vec![HostPortMapping {
                host_port: 5432,
                pod_port: 15432,
            }],
            ..NetworkPolicy::default()
        };
        let requested = SocketAddr::new(IpAddr::V4(VIRTUAL_HOST_ADDRESS), 15432);
        let dynamic = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 6432);
        assert_eq!(
            tcp_destination(requested, &policy, Some(dynamic), "outer.internal")
                .unwrap()
                .0,
            TcpDestination::Socket(dynamic)
        );
    }
}
