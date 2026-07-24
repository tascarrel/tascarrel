//! Semantic DNS resolver and attributed TCP relay owned by guestd.

use std::collections::BTreeMap;
use std::io;
use std::net::Ipv4Addr;
use std::net::SocketAddr;
use std::net::SocketAddrV4;
use std::net::TcpListener as StdTcpListener;
use std::net::UdpSocket as StdUdpSocket;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use hickory_proto::op::Message;
use hickory_proto::op::MessageType;
use hickory_proto::op::OpCode;
use hickory_proto::op::ResponseCode;
use nix::sys::socket::SockaddrIn;
use nix::sys::socket::getsockopt;
use nix::sys::socket::sockopt::OriginalDst;
use reportify::ErrorExt as _;
use reportify::Report;
use tascarrel_mux::MuxHandle;
use tascarrel_protocol::ErrorCode;
use tascarrel_protocol::Framed;
use tascarrel_protocol::Pod;
use tascarrel_protocol::RemoteError;
use tascarrel_protocol::network::DnsClientTransport;
use tascarrel_protocol::network::DnsResolveRequest;
use tascarrel_protocol::network::DnsResolveResponse;
use tascarrel_protocol::network::MAX_DNS_MESSAGE_LEN;
use tascarrel_protocol::network::MAX_NETWORK_FRAME_LEN;
use tascarrel_protocol::network::MUX_NETWORK_DNS_ENDPOINT;
use tascarrel_protocol::network::MUX_NETWORK_TCP_ENDPOINT;
use tascarrel_protocol::network::NetworkSource;
use tascarrel_protocol::network::TcpFlowOpenRequest;
use tascarrel_protocol::network::TcpFlowOpenResponse;
use thiserror::Error;
use tokio::io::AsyncReadExt as _;
use tokio::io::AsyncWriteExt as _;
use tokio::io::copy_bidirectional;
use tokio::net::TcpListener;
use tokio::net::TcpStream;
use tokio::net::UdpSocket;
use tokio::sync::Mutex;
use tokio::sync::Semaphore;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::task::JoinSet;
use tokio::time::timeout;
use tracing::debug;
use tracing::warn;

use super::NetworkBinding;
use super::NetworkFirewall;
use super::firewall::proxy_port_candidates;

const DEFAULT_MUX_WAIT_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_DNS_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_MAX_TCP_FLOWS_PER_PRINCIPAL: NonZeroUsize =
    NonZeroUsize::new(1024).expect("the default TCP flow limit is non-zero");
const DEFAULT_MAX_DNS_REQUESTS_PER_PRINCIPAL: NonZeroUsize =
    NonZeroUsize::new(64).expect("the default DNS request limit is non-zero");

#[derive(Clone, Debug)]
pub struct GuestNetworkServiceConfig {
    /// Immutable `ip` executable used to maintain the dummy default route.
    pub ip: std::path::PathBuf,
    /// Immutable `nft` executable used to replace the Tascarrel ruleset.
    pub nft: std::path::PathBuf,
    /// Maximum time to wait for an attached host mux.
    pub mux_wait_timeout: Duration,
    /// Maximum time to open and complete a semantic DNS exchange.
    pub dns_request_timeout: Duration,
    /// Maximum number of concurrent TCP flows for one attributed principal.
    pub max_tcp_flows_per_principal: NonZeroUsize,
    /// Maximum number of concurrent DNS requests for one attributed principal.
    pub max_dns_requests_per_principal: NonZeroUsize,
}

impl Default for GuestNetworkServiceConfig {
    fn default() -> Self {
        Self {
            ip: "/run/current-system/sw/bin/ip".into(),
            nft: "/run/current-system/sw/bin/nft".into(),
            mux_wait_timeout: DEFAULT_MUX_WAIT_TIMEOUT,
            dns_request_timeout: DEFAULT_DNS_REQUEST_TIMEOUT,
            max_tcp_flows_per_principal: DEFAULT_MAX_TCP_FLOWS_PER_PRINCIPAL,
            max_dns_requests_per_principal: DEFAULT_MAX_DNS_REQUESTS_PER_PRINCIPAL,
        }
    }
}

/// Failure while constructing the guest-owned network service.
#[derive(Debug, Error)]
pub enum GuestNetworkServiceError {
    #[error("guest network service configuration is invalid")]
    InvalidConfiguration,
}

#[derive(Debug, Error)]
enum GuestNetworkTransportError {
    #[error("guest network protocol failed: {0}")]
    Protocol(String),
    #[error("guest network transport failed: {0}")]
    Transport(String),
    #[error("guest network request timed out")]
    TimedOut,
}

pub struct GuestNetworkService {
    firewall: NetworkFirewall,
    mux: watch::Sender<Option<MuxHandle>>,
    principals: Mutex<BTreeMap<tascarrel_protocol::PodId, PrincipalRuntime>>,
    mux_wait_timeout: Duration,
    dns_request_timeout: Duration,
    max_tcp_flows_per_principal: NonZeroUsize,
    max_dns_requests_per_principal: NonZeroUsize,
}

struct PrincipalRuntime {
    principal: Principal,
    binding: NetworkBinding,
    tcp: JoinHandle<()>,
    dns_udp: JoinHandle<()>,
}

impl Drop for PrincipalRuntime {
    fn drop(&mut self) {
        self.tcp.abort();
        self.dns_udp.abort();
    }
}

#[derive(Clone)]
struct Principal {
    pod: Pod,
    source: NetworkSource,
}

enum CaptureOrigin<'a> {
    Veth(&'a str, Ipv4Addr),
    BuildVeth(&'a str, Ipv4Addr),
    Guest,
    System,
}

impl CaptureOrigin<'_> {
    /// Selects the proxy bind address for this capture origin.
    ///
    /// Guest-local UDP redirects target loopback. Binding their unconnected
    /// sockets to the wildcard address can select the dummy interface address
    /// for replies, which does not match conntrack's translated tuple.
    fn proxy_bind_address(&self) -> Ipv4Addr {
        match self {
            Self::Veth(..) | Self::BuildVeth(..) => Ipv4Addr::UNSPECIFIED,
            Self::Guest | Self::System => Ipv4Addr::LOCALHOST,
        }
    }
}

impl GuestNetworkService {
    /// Creates an idle guest network service.
    ///
    /// # Errors
    ///
    /// Returns an error when a configured timeout is zero.
    pub fn new(
        config: GuestNetworkServiceConfig,
    ) -> Result<Arc<Self>, Report<GuestNetworkServiceError>> {
        if config.mux_wait_timeout.is_zero() || config.dns_request_timeout.is_zero() {
            return Err(GuestNetworkServiceError::InvalidConfiguration.report());
        }
        let (mux, _) = watch::channel(None);
        Ok(Arc::new(Self {
            firewall: NetworkFirewall::new(config.ip, config.nft),
            mux,
            principals: Mutex::new(BTreeMap::new()),
            mux_wait_timeout: config.mux_wait_timeout,
            dns_request_timeout: config.dns_request_timeout,
            max_tcp_flows_per_principal: config.max_tcp_flows_per_principal,
            max_dns_requests_per_principal: config.max_dns_requests_per_principal,
        }))
    }

    /// Installs the initial fail-closed guest firewall state.
    ///
    /// # Errors
    ///
    /// Returns an error when guest network commands cannot install the rules.
    #[tracing::instrument(level = "debug", skip(self), err(Debug))]
    pub async fn initialize(&self) -> Result<(), RemoteError> {
        self.firewall.sync(&[]).await.map_err(firewall_error)
    }

    pub fn attach_mux(&self, mux: MuxHandle) {
        self.mux.send_replace(Some(mux));
    }

    pub fn detach_mux(&self) {
        self.mux.send_replace(None);
    }

    /// Opens one private service channel on the currently attached host mux.
    ///
    /// # Errors
    ///
    /// Returns an error when no host mux becomes available or the endpoint
    /// cannot be opened.
    pub async fn open_channel(
        &self,
        endpoint: &str,
    ) -> Result<tascarrel_mux::Channel, RemoteError> {
        self.mux().await?.open(endpoint).await.map_err(|error| {
            RemoteError::new(
                ErrorCode::ExecutionFailed,
                format!("open multiplexed service channel: {error}"),
            )
        })
    }

    async fn mux(&self) -> Result<MuxHandle, RemoteError> {
        let mut receiver = self.mux.subscribe();
        timeout(self.mux_wait_timeout, async {
            loop {
                if let Some(mux) = receiver.borrow().clone() {
                    return Ok(mux);
                }
                receiver.changed().await.map_err(|_| {
                    RemoteError::new(ErrorCode::Internal, "network multiplexer is unavailable")
                })?;
            }
        })
        .await
        .map_err(|_| {
            RemoteError::new(
                ErrorCode::ExecutionFailed,
                "timed out waiting for the host network service",
            )
        })?
    }

    #[tracing::instrument(
        name = "tascarrel_guest.network.activate_principal",
        level = "debug",
        skip(self, pod, origin),
        fields(principal = %pod.id),
        err(Debug)
    )]
    async fn activate_principal(
        self: &Arc<Self>,
        pod: &Pod,
        origin: CaptureOrigin<'_>,
    ) -> Result<NetworkBinding, RemoteError> {
        let mut principals = self.principals.lock().await;
        if principals.contains_key(&pod.id) {
            return Err(RemoteError::new(
                ErrorCode::AlreadyExists,
                "network listeners already exist for this principal",
            ));
        }
        let (proxy_port, tcp, dns_udp) = bind_proxy_listeners(origin.proxy_bind_address())?;
        let (binding, source) = match origin {
            CaptureOrigin::Veth(interface, address) => (
                NetworkBinding::for_veth(pod.uid, proxy_port, interface, address)?,
                NetworkSource::Pod(pod.id.clone()),
            ),
            CaptureOrigin::BuildVeth(interface, address) => (
                NetworkBinding::for_build_veth(proxy_port, interface, address)?,
                NetworkSource::ImageBuild,
            ),
            CaptureOrigin::Guest => (
                NetworkBinding::for_guest(proxy_port)?,
                NetworkSource::WorkspaceService,
            ),
            CaptureOrigin::System => (
                NetworkBinding::for_system(pod.uid, proxy_port)?,
                NetworkSource::WorkspaceService,
            ),
        };
        let principal = Principal {
            pod: pod.clone(),
            source,
        };
        let tcp_service = Arc::clone(self);
        let tcp_principal = principal.clone();
        let tcp = tokio::spawn(async move {
            tcp_service.run_tcp_listener(tcp_principal, tcp).await;
        });
        let dns_service = Arc::clone(self);
        let dns_principal = principal.clone();
        let dns_udp = tokio::spawn(async move {
            dns_service
                .run_dns_udp_listener(dns_principal, dns_udp)
                .await;
        });
        principals.insert(
            pod.id.clone(),
            PrincipalRuntime {
                principal,
                binding: binding.clone(),
                tcp,
                dns_udp,
            },
        );
        let active = principals
            .values()
            .map(|runtime| runtime.binding.clone())
            .collect::<Vec<_>>();
        if let Err(error) = self.firewall.sync(&active).await {
            principals.remove(&pod.id);
            return Err(firewall_error(error));
        }
        Ok(binding)
    }

    /// Activates attributed networking for one pod veth.
    ///
    /// # Errors
    ///
    /// Returns an error when listeners, attribution, or firewall installation
    /// fails.
    pub async fn activate_veth(
        self: &Arc<Self>,
        pod: &Pod,
        input_interface: &str,
        pod_address: Ipv4Addr,
    ) -> Result<NetworkBinding, RemoteError> {
        self.activate_principal(pod, CaptureOrigin::Veth(input_interface, pod_address))
            .await
    }

    /// Activates attributed networking for one isolated image build veth.
    ///
    /// # Errors
    ///
    /// Returns an error when the principal is invalid or networking cannot be
    /// installed.
    pub async fn activate_build_veth(
        self: &Arc<Self>,
        principal: &Pod,
        input_interface: &str,
        build_address: Ipv4Addr,
    ) -> Result<NetworkBinding, RemoteError> {
        if principal.uid != 0 || principal.gid != 0 {
            return Err(RemoteError::new(
                ErrorCode::InvalidRequest,
                "image build principal must use guest root metadata",
            ));
        }
        self.activate_principal(
            principal,
            CaptureOrigin::BuildVeth(input_interface, build_address),
        )
        .await
    }

    /// Activates attributed networking for one stable guest service UID.
    ///
    /// # Errors
    ///
    /// Returns an error when listeners, attribution, or firewall installation
    /// fails.
    pub async fn activate_system(
        self: &Arc<Self>,
        principal: &Pod,
    ) -> Result<NetworkBinding, RemoteError> {
        self.activate_principal(principal, CaptureOrigin::System)
            .await
    }

    /// Activates the guest-wide fallback used for transient system UIDs such
    /// as Nix build users.
    ///
    /// # Errors
    ///
    /// Returns an error when listeners or firewall installation fails.
    pub async fn activate_guest(
        self: &Arc<Self>,
        principal: &Pod,
    ) -> Result<NetworkBinding, RemoteError> {
        self.activate_principal(principal, CaptureOrigin::Guest)
            .await
    }

    /// Removes one principal's firewall binding and listeners.
    ///
    /// # Errors
    ///
    /// Returns an error when the replacement firewall state cannot be
    /// installed.
    #[tracing::instrument(
        name = "tascarrel_guest.network.deactivate_principal",
        level = "debug",
        skip(self, principal, _binding),
        fields(principal = %principal.id),
        err(Debug)
    )]
    pub async fn deactivate(
        &self,
        principal: &Pod,
        _binding: &NetworkBinding,
    ) -> Result<(), RemoteError> {
        let mut principals = self.principals.lock().await;
        let active = principals
            .values()
            .filter(|runtime| runtime.principal.pod.id != principal.id)
            .map(|runtime| runtime.binding.clone())
            .collect::<Vec<_>>();
        self.firewall.sync(&active).await.map_err(firewall_error)?;
        principals.remove(&principal.id);
        Ok(())
    }

    async fn run_tcp_listener(self: Arc<Self>, principal: Principal, listener: TcpListener) {
        let permits = Arc::new(Semaphore::new(self.max_tcp_flows_per_principal.get()));
        let mut flows = JoinSet::new();
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let (stream, source) = match accepted {
                        Ok(value) => value,
                        Err(error) => {
                            warn!(principal = %principal.pod.id, %error, "network TCP listener failed");
                            return;
                        }
                    };
                    let Ok(permit) = Arc::clone(&permits).try_acquire_owned() else {
                        debug!(principal = %principal.pod.id, "rejecting TCP flow above principal limit");
                        continue;
                    };
                    let destination = match original_tcp_destination(&stream) {
                        Ok(value) => value,
                        Err(error) => {
                            debug!(principal = %principal.pod.id, %error, "TCP flow lacks an original destination");
                            continue;
                        }
                    };
                    let service = Arc::clone(&self);
                    let principal = principal.clone();
                    flows.spawn(async move {
                        let _permit = permit;
                        let result = if destination.port() == 53 {
                            service.serve_dns_tcp(&principal, stream).await
                        } else {
                            service.forward_tcp(&principal, source, destination, stream).await
                        };
                        if let Err(error) = result {
                            debug!(principal = %principal.pod.id, %source, %destination, %error, "TCP flow closed");
                        }
                    });
                }
                Some(result) = flows.join_next(), if !flows.is_empty() => {
                    if let Err(error) = result {
                        debug!(principal = %principal.pod.id, %error, "TCP network task failed");
                    }
                }
            }
        }
    }

    #[tracing::instrument(
        name = "tascarrel_guest.network.forward_tcp",
        level = "debug",
        skip(self, principal, local),
        fields(principal = %principal.pod.id),
        err(Debug)
    )]
    async fn forward_tcp(
        &self,
        principal: &Principal,
        source_address: SocketAddr,
        destination: SocketAddr,
        mut local: TcpStream,
    ) -> Result<(), Report<GuestNetworkTransportError>> {
        let channel = self
            .mux()
            .await
            .map_err(transport_error)?
            .open(MUX_NETWORK_TCP_ENDPOINT)
            .await
            .map_err(transport_error)?;
        let mut framed =
            Framed::with_max_frame_len(channel, MAX_NETWORK_FRAME_LEN).map_err(protocol_error)?;
        framed
            .write(&TcpFlowOpenRequest {
                source: principal.source.clone(),
                source_address,
                destination,
            })
            .await
            .map_err(protocol_error)?;
        let response = framed
            .read::<TcpFlowOpenResponse>()
            .await
            .map_err(protocol_error)?
            .ok_or_else(|| protocol_error("host closed TCP channel before its response"))?;
        response.validate().map_err(protocol_error)?;
        response
            .result
            .map_err(|error| protocol_error(error.message))?;
        let mut channel = framed.into_inner();
        copy_bidirectional(&mut local, &mut channel)
            .await
            .map_err(transport_error)?;
        Ok(())
    }

    async fn run_dns_udp_listener(self: Arc<Self>, principal: Principal, socket: UdpSocket) {
        let socket = Arc::new(socket);
        let permits = Arc::new(Semaphore::new(self.max_dns_requests_per_principal.get()));
        let mut requests = JoinSet::new();
        let mut buffer = vec![0_u8; MAX_DNS_MESSAGE_LEN];
        loop {
            tokio::select! {
                received = socket.recv_from(&mut buffer) => {
                    let (length, peer) = match received {
                        Ok(value) => value,
                        Err(error) => {
                            warn!(principal = %principal.pod.id, %error, "network DNS UDP listener failed");
                            return;
                        }
                    };
                    let Ok(permit) = Arc::clone(&permits).try_acquire_owned() else {
                        debug!(principal = %principal.pod.id, "dropping DNS request above principal limit");
                        continue;
                    };
                    let payload = buffer[..length].to_vec();
                    let socket = Arc::clone(&socket);
                    let service = Arc::clone(&self);
                    let principal = principal.clone();
                    requests.spawn(async move {
                        let _permit = permit;
                        match service.resolve_dns(&principal, DnsClientTransport::Udp, &payload).await {
                            Ok(response) => {
                                let response = udp_response(&payload, response);
                                if let Err(error) = socket.send_to(&response, peer).await {
                                    debug!(principal = %principal.pod.id, %peer, %error, "could not send DNS UDP response");
                                }
                            }
                            Err(error) => debug!(principal = %principal.pod.id, %peer, %error, "DNS UDP request failed"),
                        }
                    });
                }
                Some(result) = requests.join_next(), if !requests.is_empty() => {
                    if let Err(error) = result {
                        debug!(principal = %principal.pod.id, %error, "DNS UDP task failed");
                    }
                }
            }
        }
    }

    async fn serve_dns_tcp(
        &self,
        principal: &Principal,
        mut stream: TcpStream,
    ) -> Result<(), Report<GuestNetworkTransportError>> {
        loop {
            let length = match stream.read_u16().await {
                Ok(length) => usize::from(length),
                Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
                Err(error) => return Err(transport_error(error)),
            };
            if length == 0 || length > MAX_DNS_MESSAGE_LEN {
                return Err(protocol_error("invalid DNS TCP length"));
            }
            let mut payload = vec![0_u8; length];
            stream
                .read_exact(&mut payload)
                .await
                .map_err(transport_error)?;
            let response = self
                .resolve_dns(principal, DnsClientTransport::Tcp, &payload)
                .await?;
            let length = u16::try_from(response.len()).map_err(protocol_error)?;
            stream.write_u16(length).await.map_err(transport_error)?;
            stream.write_all(&response).await.map_err(transport_error)?;
        }
    }

    #[tracing::instrument(
        name = "tascarrel_guest.network.resolve_dns",
        level = "debug",
        skip(self, principal, payload),
        fields(principal = %principal.pod.id),
        err(Debug)
    )]
    async fn resolve_dns(
        &self,
        principal: &Principal,
        transport: DnsClientTransport,
        payload: &[u8],
    ) -> Result<Vec<u8>, Report<GuestNetworkTransportError>> {
        let query = Message::from_vec(payload).map_err(protocol_error)?;
        let semantic = semantic_dns_request(&query, principal.source.clone(), transport)?;
        let channel = timeout(
            self.dns_request_timeout,
            self.mux()
                .await
                .map_err(transport_error)?
                .open(MUX_NETWORK_DNS_ENDPOINT),
        )
        .await
        .map_err(|_| GuestNetworkTransportError::TimedOut.report())?
        .map_err(transport_error)?;
        let mut framed =
            Framed::with_max_frame_len(channel, MAX_NETWORK_FRAME_LEN).map_err(protocol_error)?;
        framed.write(&semantic).await.map_err(protocol_error)?;
        framed.get_mut().shutdown().await.map_err(transport_error)?;
        let response = timeout(
            self.dns_request_timeout,
            framed.read::<DnsResolveResponse>(),
        )
        .await
        .map_err(|_| GuestNetworkTransportError::TimedOut.report())?
        .map_err(protocol_error)?
        .ok_or_else(|| protocol_error("host closed DNS channel before its response"))?;
        response.validate().map_err(protocol_error)?;
        let trailing = timeout(
            self.dns_request_timeout,
            framed.read::<DnsResolveResponse>(),
        )
        .await
        .map_err(|_| GuestNetworkTransportError::TimedOut.report())?
        .map_err(protocol_error)?;
        if trailing.is_some() {
            return Err(protocol_error("host sent more than one DNS response"));
        }
        let mut answer = match response.result {
            Ok(message) => Message::from_vec(&message).map_err(protocol_error)?,
            Err(_) => dns_error_response(&query, ResponseCode::ServFail),
        };
        answer.metadata.id = query.metadata.id;
        answer.metadata.recursion_desired = query.metadata.recursion_desired;
        answer.queries.clone_from(&query.queries);
        answer.edns.clone_from(&query.edns);
        let dnssec_ok = query
            .edns
            .as_ref()
            .is_some_and(|edns| edns.flags().dnssec_ok);
        answer = answer.maybe_strip_dnssec_records(dnssec_ok);
        answer.to_vec().map_err(protocol_error)
    }
}

fn semantic_dns_request(
    message: &Message,
    source: NetworkSource,
    transport: DnsClientTransport,
) -> Result<DnsResolveRequest, Report<GuestNetworkTransportError>> {
    if message.metadata.message_type != MessageType::Query
        || message.metadata.op_code != OpCode::Query
        || message.queries.len() != 1
    {
        return Err(protocol_error(
            "DNS request must contain exactly one standard query",
        ));
    }
    let query = &message.queries[0];
    let request = DnsResolveRequest {
        source,
        transport,
        name: query.name().to_utf8(),
        record_type: u16::from(query.query_type()),
        record_class: u16::from(query.query_class()),
        recursion_desired: message.metadata.recursion_desired,
        dnssec_ok: message
            .edns
            .as_ref()
            .is_some_and(|edns| edns.flags().dnssec_ok),
    };
    request.validate().map_err(protocol_error)?;
    Ok(request)
}

fn protocol_error(error: impl std::fmt::Display) -> Report<GuestNetworkTransportError> {
    GuestNetworkTransportError::Protocol(error.to_string()).report()
}

fn transport_error(error: impl std::fmt::Display) -> Report<GuestNetworkTransportError> {
    GuestNetworkTransportError::Transport(error.to_string()).report()
}

fn dns_error_response(query: &Message, response_code: ResponseCode) -> Message {
    let mut response = Message::error_msg(query.metadata.id, OpCode::Query, response_code);
    response.metadata.recursion_desired = query.metadata.recursion_desired;
    response.metadata.recursion_available = true;
    response.queries.clone_from(&query.queries);
    response.edns.clone_from(&query.edns);
    response
}

fn udp_response(query_payload: &[u8], response: Vec<u8>) -> Vec<u8> {
    let Ok(query) = Message::from_vec(query_payload) else {
        return response;
    };
    let limit = query
        .edns
        .as_ref()
        .map_or(512, |edns| usize::from(edns.max_payload()));
    if response.len() <= limit {
        return response;
    }
    Message::from_vec(&response)
        .ok()
        .and_then(|message| message.truncate().to_vec().ok())
        .unwrap_or(response)
}

fn bind_proxy_listeners(
    bind_address: Ipv4Addr,
) -> Result<(u16, TcpListener, UdpSocket), RemoteError> {
    for proxy_port in proxy_port_candidates() {
        let address = SocketAddrV4::new(bind_address, proxy_port);
        let tcp = match StdTcpListener::bind(address) {
            Ok(listener) => listener,
            Err(error) if error.kind() == io::ErrorKind::AddrInUse => continue,
            Err(error) => return Err(proxy_bind_error(&error)),
        };
        let udp = match StdUdpSocket::bind(address) {
            Ok(socket) => socket,
            Err(error) if error.kind() == io::ErrorKind::AddrInUse => continue,
            Err(error) => return Err(proxy_bind_error(&error)),
        };
        tcp.set_nonblocking(true)
            .map_err(|error| proxy_bind_error(&error))?;
        udp.set_nonblocking(true)
            .map_err(|error| proxy_bind_error(&error))?;
        return Ok((
            proxy_port,
            TcpListener::from_std(tcp).map_err(|error| proxy_bind_error(&error))?,
            UdpSocket::from_std(udp).map_err(|error| proxy_bind_error(&error))?,
        ));
    }
    Err(RemoteError::new(
        ErrorCode::ResourceExhausted,
        "no privileged network proxy ports are available",
    ))
}

fn original_tcp_destination(stream: &TcpStream) -> io::Result<SocketAddr> {
    let address = getsockopt(stream, OriginalDst).map_err(errno_to_io)?;
    let address = SockaddrIn::from(address);
    Ok(SocketAddr::V4(SocketAddrV4::new(
        address.ip(),
        address.port(),
    )))
}

fn proxy_bind_error(error: &io::Error) -> RemoteError {
    RemoteError::new(
        ErrorCode::Internal,
        format!("could not bind network proxy: {error}"),
    )
}

fn firewall_error(error: impl std::fmt::Display) -> RemoteError {
    RemoteError::new(
        ErrorCode::Internal,
        format!("could not update network firewall: {error}"),
    )
}

fn errno_to_io(error: nix::errno::Errno) -> io::Error {
    io::Error::from_raw_os_error(error as i32)
}

#[cfg(test)]
mod tests {
    use hickory_proto::op::Query;
    use hickory_proto::rr::Name;
    use hickory_proto::rr::RecordType;

    use super::*;

    /// Verifies semantic conversion preserves the supported DNS question and
    /// request flags.
    #[test]
    fn semantic_request_preserves_question_and_flags() {
        let mut message = Message::query();
        message.metadata.recursion_desired = true;
        message.add_query(Query::query(
            Name::from_ascii("example.com.").unwrap(),
            RecordType::AAAA,
        ));
        let request = semantic_dns_request(
            &message,
            NetworkSource::WorkspaceService,
            DnsClientTransport::Udp,
        )
        .unwrap();
        assert_eq!(request.name, "example.com.");
        assert_eq!(request.record_type, u16::from(RecordType::AAAA));
        assert!(request.recursion_desired);
    }

    /// Verifies UDP responses exceeding the advertised payload are marked as
    /// truncated.
    #[test]
    fn oversized_udp_response_is_truncated() {
        let mut query = Message::query();
        query.add_query(Query::query(
            Name::from_ascii("example.com.").unwrap(),
            RecordType::A,
        ));
        let mut response = dns_error_response(&query, ResponseCode::NoError);
        response.add_answers((0..100).map(|index| {
            hickory_proto::rr::Record::from_rdata(
                Name::from_ascii(format!("host-{index}.example.com.")).unwrap(),
                60,
                hickory_proto::rr::RData::A(hickory_proto::rr::rdata::A(Ipv4Addr::LOCALHOST)),
            )
        }));
        let payload = udp_response(&query.to_vec().unwrap(), response.to_vec().unwrap());
        let decoded = Message::from_vec(&payload).unwrap();
        assert!(decoded.metadata.truncation);
        assert!(payload.len() <= 512);
    }
}
