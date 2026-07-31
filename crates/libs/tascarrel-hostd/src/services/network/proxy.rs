//! HTTP and TLS policy enforcement for attributed guest TCP flows.
//!
//! [`HttpProxy`] resolves admitted hostnames on the host. It relays external
//! TLS unchanged unless the connection's SNI matches an HTTPS secret-injection
//! rule. HTTP and HTTPS connections to exposed host services are always
//! mediated so the proxy can inject secrets and rewrite request authority to
//! localhost.

use std::convert::Infallible;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::task::Context as TaskContext;
use std::task::Poll;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use bytes::Bytes;
use http_body_util::BodyExt;
use http_body_util::Full;
use http_body_util::Limited;
use http_body_util::combinators::BoxBody;
use hyper::Method;
use hyper::Request;
use hyper::Response;
use hyper::StatusCode;
use hyper::Uri;
use hyper::body::Incoming;
use hyper::client::conn::http1 as client_http1;
use hyper::header::CONNECTION;
use hyper::header::CONTENT_ENCODING;
use hyper::header::CONTENT_TYPE;
use hyper::header::HOST;
use hyper::header::HeaderName;
use hyper::header::HeaderValue;
use hyper::header::ORIGIN;
use hyper::header::UPGRADE;
use hyper::http::uri::Authority;
use hyper::server::conn::http1 as server_http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use reportify::ErrorExt as _;
use reportify::Report;
use rustls::ClientConfig;
use rustls::RootCertStore;
use rustls::pki_types::ServerName;
use tascarrel_api::types::workspaces::WorkspaceName;
use tascarrel_protocol::network::VIRTUAL_HOSTNAME;
use thiserror::Error;
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;
use tokio::io::ReadBuf;
use tokio::io::copy_bidirectional;
use tokio::net::TcpStream;
use tokio::net::lookup_host;
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio_rustls::LazyConfigAcceptor;
use tokio_rustls::TlsConnector;
use tracing::debug;

use super::policy::MAX_SECRET_BYTES;
use super::policy::NetworkPolicy;
use super::policy::SecretInjection;
use super::policy::forbidden_secret_header;
use super::service::HttpRequestRecorder;
use crate::WorkspaceAuthority;
use crate::services::secrets::SecretsService;

type ProxyBody = BoxBody<Bytes, Box<dyn std::error::Error + Send + Sync>>;
type ProxyResult<T> = Result<T, Report<HttpProxyError>>;
type UpgradeTask = Arc<Mutex<Option<JoinHandle<ProxyResult<()>>>>>;

const MAX_GRAPHQL_REQUEST_BYTES: usize = 256 * 1024;

struct HttpConnectionContext {
    port: u16,
    tls_host: Option<String>,
    upstream_tls: bool,
    workspace_name: WorkspaceName,
    secrets: SecretsService,
    request_recorder: HttpRequestRecorder,
}

#[derive(Clone, Debug)]
enum HttpProxyTarget {
    RequestHost,
    HostPort { connection_host: String },
}

#[derive(Debug, Error)]
pub(crate) enum HttpProxyError {
    #[error("HTTP proxy failed: {0}")]
    Failed(String),
}

struct ForwardContext<T> {
    port: u16,
    tls_host: Option<String>,
    upstream_tls: bool,
    cleanup_io: SharedIo<T>,
    upgraded: Arc<AtomicBool>,
    upgrade_task: UpgradeTask,
}

struct HttpRequestAudit {
    occurred_at: jiff::Timestamp,
    host: Option<String>,
    method: Method,
    path: String,
    secrets_injected: bool,
}

impl HttpRequestAudit {
    fn new(request: &Request<Incoming>) -> Self {
        Self {
            occurred_at: jiff::Timestamp::now(),
            host: None,
            method: request.method().clone(),
            path: request.uri().path().to_owned(),
            secrets_injected: false,
        }
    }
}

struct SharedIo<T> {
    inner: Arc<Mutex<T>>,
}

impl<T> SharedIo<T> {
    fn new(io: T) -> Self {
        Self {
            inner: Arc::new(Mutex::new(io)),
        }
    }
}

impl<T> Clone for SharedIo<T> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<T: AsyncRead + Unpin> AsyncRead for SharedIo<T> {
    fn poll_read(
        self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.inner.lock() {
            Ok(mut io) => Pin::new(&mut *io).poll_read(context, buffer),
            Err(_) => Poll::Ready(Err(io::Error::other("proxy stream lock is poisoned"))),
        }
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for SharedIo<T> {
    fn poll_write(
        self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.inner.lock() {
            Ok(mut io) => Pin::new(&mut *io).poll_write(context, bytes),
            Err(_) => Poll::Ready(Err(io::Error::other("proxy stream lock is poisoned"))),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, context: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        match self.inner.lock() {
            Ok(mut io) => Pin::new(&mut *io).poll_flush(context),
            Err(_) => Poll::Ready(Err(io::Error::other("proxy stream lock is poisoned"))),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, context: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        match self.inner.lock() {
            Ok(mut io) => Pin::new(&mut *io).poll_shutdown(context),
            Err(_) => Poll::Ready(Err(io::Error::other("proxy stream lock is poisoned"))),
        }
    }
}

/// Enforces hostname policy and secret injection for one attributed TCP flow.
#[derive(Clone)]
pub(crate) struct HttpProxy {
    policy: NetworkPolicy,
    authority: Option<Arc<WorkspaceAuthority>>,
    interception_client_tls: Option<Arc<ClientConfig>>,
    connect_timeout: Duration,
    target: HttpProxyTarget,
}

impl std::fmt::Debug for HttpProxy {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HttpProxy")
            .field("policy", &self.policy)
            .field("authority", &self.authority)
            .field("connect_timeout", &self.connect_timeout)
            .field("target", &self.target)
            .finish_non_exhaustive()
    }
}

impl HttpProxy {
    pub fn new(
        policy: NetworkPolicy,
        authority: Option<Arc<WorkspaceAuthority>>,
        connect_timeout: Duration,
    ) -> Self {
        let interception_client_tls = authority.as_deref().map(interception_client_config);
        Self {
            policy,
            authority,
            interception_client_tls,
            connect_timeout,
            target: HttpProxyTarget::RequestHost,
        }
    }

    /// Creates a proxy for one explicitly exposed host service.
    pub fn for_host_port(
        policy: NetworkPolicy,
        authority: Option<Arc<WorkspaceAuthority>>,
        connect_timeout: Duration,
        connection_host: String,
    ) -> Self {
        let interception_client_tls = authority.as_deref().map(interception_client_config);
        Self {
            policy,
            authority,
            interception_client_tls,
            connect_timeout,
            target: HttpProxyTarget::HostPort { connection_host },
        }
    }

    pub async fn serve_http<T>(
        self,
        channel: T,
        port: u16,
        workspace_name: WorkspaceName,
        secrets: SecretsService,
        request_recorder: HttpRequestRecorder,
    ) -> ProxyResult<()>
    where
        T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        self.serve_connection(
            channel,
            HttpConnectionContext {
                port,
                tls_host: None,
                upstream_tls: false,
                workspace_name,
                secrets,
                request_recorder,
            },
        )
        .await
    }

    pub async fn serve_https<T>(
        self,
        channel: T,
        port: u16,
        workspace_name: WorkspaceName,
        secrets: SecretsService,
        request_recorder: HttpRequestRecorder,
    ) -> ProxyResult<()>
    where
        T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let start = timeout(
            self.connect_timeout,
            LazyConfigAcceptor::new(
                rustls::server::Acceptor::default(),
                ClientHelloRecordingIo::new(channel),
            ),
        )
        .await
        .map_err(|_| proxy_error("timed out reading TLS ClientHello"))?
        .map_err(|error| proxy_error(format!("failed to read TLS ClientHello: {error}")))?;
        let host = start
            .client_hello()
            .server_name()
            .ok_or_else(|| proxy_error("TLS SNI is required"))?
            .to_ascii_lowercase();
        self.require_host(&host)?;
        if !self.intercepts_tls_for_host(&host) {
            return self.relay_tls(start.io, &host, port).await;
        }
        let authority = self
            .authority
            .as_ref()
            .ok_or_else(|| proxy_error("workspace HTTPS authority is unavailable"))?;
        let server = authority
            .server_config(&host)
            .map_err(|error| proxy_error(format!("failed to issue TLS certificate: {error}")))?;
        let mut start = start;
        start.io.discard_recording();
        let stream = start.into_stream(server).await.map_err(|error| {
            proxy_error(format!(
                "failed to complete pod-facing TLS handshake: {error}"
            ))
        })?;
        self.serve_connection(
            stream,
            HttpConnectionContext {
                port,
                tls_host: Some(host),
                upstream_tls: true,
                workspace_name,
                secrets,
                request_recorder,
            },
        )
        .await
    }

    /// Resolves an admitted SNI and relays the original TLS stream unchanged.
    #[tracing::instrument(
        name = "tascarrel_host.network.relay_tls",
        level = "debug",
        skip(self, channel),
        err(Debug)
    )]
    async fn relay_tls<T>(
        &self,
        mut channel: ClientHelloRecordingIo<T>,
        host: &str,
        port: u16,
    ) -> ProxyResult<()>
    where
        T: AsyncRead + AsyncWrite + Unpin,
    {
        let client_hello = channel.finish_recording();
        let mut upstream = self.connect(host, port).await?;
        upstream.write_all(&client_hello).await.map_err(|error| {
            proxy_error(format!(
                "failed to forward upstream TLS ClientHello: {error}"
            ))
        })?;
        if let Err(error) = copy_bidirectional(&mut channel, &mut upstream).await {
            if !is_closed_connection_error(&error) {
                return Err(proxy_error(format!(
                    "failed to relay TLS connection: {error}"
                )));
            }
            debug!(%error, "TLS relay peer closed the connection");
        }
        Ok(())
    }

    async fn serve_connection<T>(self, io: T, context: HttpConnectionContext) -> ProxyResult<()>
    where
        T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let HttpConnectionContext {
            port,
            tls_host,
            upstream_tls,
            workspace_name,
            secrets,
            request_recorder,
        } = context;
        let drain_timeout = self.connect_timeout;
        let io = SharedIo::new(io);
        let cleanup_io = io.clone();
        let upgraded = Arc::new(AtomicBool::new(false));
        let upgrade_task: UpgradeTask = Arc::new(Mutex::new(None));
        let service_cleanup_io = cleanup_io.clone();
        let service_upgraded = Arc::clone(&upgraded);
        let service_upgrade_task = Arc::clone(&upgrade_task);
        let proxy = Arc::new(self);
        let connection = server_http1::Builder::new()
            .keep_alive(true)
            .serve_connection(
                TokioIo::new(io),
                service_fn(move |request| {
                    let proxy = Arc::clone(&proxy);
                    let tls_host = tls_host.clone();
                    let cleanup_io = service_cleanup_io.clone();
                    let upgraded = Arc::clone(&service_upgraded);
                    let upgrade_task = Arc::clone(&service_upgrade_task);
                    let workspace_name = workspace_name.clone();
                    let secrets = secrets.clone();
                    let request_recorder = request_recorder.clone();
                    async move {
                        let mut audit = HttpRequestAudit::new(&request);
                        let context = ForwardContext {
                            port,
                            tls_host,
                            upstream_tls,
                            cleanup_io,
                            upgraded,
                            upgrade_task,
                        };
                        let response = match proxy
                            .forward(request, context, &workspace_name, &secrets, &mut audit)
                            .await
                        {
                            Ok(response) => response,
                            Err(error) => {
                                debug!(%error, "HTTP network request failed");
                                error_response(&error)
                            }
                        };
                        request_recorder.record(
                            audit.occurred_at,
                            audit.host,
                            &audit.method,
                            &audit.path,
                            audit.secrets_injected,
                        );
                        Ok::<_, Infallible>(response)
                    }
                }),
            );
        connection.with_upgrades().await.map_err(|error| {
            proxy_error(format!("failed to serve pod HTTP connection: {error}"))
        })?;
        if upgraded.load(Ordering::Acquire) {
            let task = upgrade_task
                .lock()
                .map_err(|_| proxy_error("upgrade task lock is poisoned"))?
                .take()
                .ok_or_else(|| proxy_error("upgraded HTTP connection has no relay task"))?;
            task.await.map_err(|error| {
                proxy_error(format!("failed to join upgraded HTTP relay: {error}"))
            })??;
        } else {
            close_and_drain(cleanup_io, drain_timeout).await?;
        }
        Ok(())
    }

    async fn forward<T>(
        &self,
        mut request: Request<Incoming>,
        context: ForwardContext<T>,
        workspace_name: &WorkspaceName,
        secrets: &SecretsService,
        audit: &mut HttpRequestAudit,
    ) -> ProxyResult<Response<ProxyBody>>
    where
        T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        debug!(method = %request.method(), "proxying HTTP request");
        if request.method() == Method::CONNECT {
            return Err(proxy_error("HTTP CONNECT tunnels are not supported"));
        }
        let wants_upgrade = requests_upgrade(request.headers())?;
        let pod_upgrade = wants_upgrade.then(|| hyper::upgrade::on(&mut request));
        let request_authority = request_authority(&request)?;
        let host = request_authority.host().to_ascii_lowercase();
        audit.host = Some(host.clone());
        self.require_host(&host)?;
        if context.tls_host.as_deref().is_some_and(|sni| sni != host) {
            return Err(proxy_error("HTTP Host does not match TLS SNI"));
        }
        let method = request.method().clone();
        let path = request.uri().path().to_owned();
        if matches!(&self.target, HttpProxyTarget::HostPort { .. }) {
            rewrite_host_port_request(request.headers_mut(), &request_authority)?;
        }
        let (mut request, secrets_injected) = self
            .apply_request_policy(request, &host, &path, &method, workspace_name, secrets)
            .await?;
        audit.secrets_injected = secrets_injected;
        strip_hop_by_hop(request.headers_mut(), wants_upgrade);
        *request.uri_mut() = request
            .uri()
            .path_and_query()
            .map_or_else(|| "/".parse(), |path| path.as_str().parse())
            .map_err(|error| proxy_error(format!("invalid HTTP request target: {error}")))?;

        let stream = self.connect(&host, context.port).await?;
        if context.upstream_tls {
            let client_tls = self
                .interception_client_tls
                .as_ref()
                .ok_or_else(|| proxy_error("workspace HTTPS authority is unavailable"))?;
            let tls_host = match &self.target {
                HttpProxyTarget::RequestHost => host.clone(),
                HttpProxyTarget::HostPort { .. } => "localhost".to_owned(),
            };
            let name = ServerName::try_from(tls_host)
                .map_err(|error| proxy_error(format!("invalid TLS server name: {error}")))?;
            let tls = TlsConnector::from(Arc::clone(client_tls))
                .connect(name, stream)
                .await
                .map_err(|error| proxy_error(format!("failed to connect upstream TLS: {error}")))?;
            send_request(
                tls,
                request,
                pod_upgrade,
                context.cleanup_io,
                context.upgraded,
                context.upgrade_task,
                self.connect_timeout,
            )
            .await
        } else {
            send_request(
                stream,
                request,
                pod_upgrade,
                context.cleanup_io,
                context.upgraded,
                context.upgrade_task,
                self.connect_timeout,
            )
            .await
        }
    }

    fn require_host(&self, host: &str) -> ProxyResult<()> {
        match &self.target {
            HttpProxyTarget::RequestHost if !self.policy.host_allowed(host) => Err(proxy_error(
                format!("host {host:?} is denied by workspace policy"),
            )),
            HttpProxyTarget::HostPort { .. } if host != VIRTUAL_HOSTNAME => Err(proxy_error(
                format!("host {host:?} does not identify the exposed host service"),
            )),
            _ => Ok(()),
        }
    }

    async fn connect(&self, host: &str, port: u16) -> ProxyResult<TcpStream> {
        if let HttpProxyTarget::HostPort { connection_host } = &self.target {
            return timeout(
                self.connect_timeout,
                TcpStream::connect((connection_host.as_str(), port)),
            )
            .await
            .map_err(|_| proxy_error("timed out connecting to exposed host service"))?
            .map_err(|error| {
                proxy_error(format!(
                    "failed to connect to exposed host service: {error}"
                ))
            });
        }
        let addresses = lookup_host((host, port))
            .await
            .map_err(|error| proxy_error(format!("failed to resolve host {host:?}: {error}")))?;
        let mut last_error = None;
        for address in addresses {
            if !self
                .policy
                .address_allowed(address.ip())
                .map_err(|error| proxy_error(error.to_string()))?
            {
                continue;
            }
            match timeout(self.connect_timeout, TcpStream::connect(address)).await {
                Ok(Ok(stream)) => return Ok(stream),
                Ok(Err(error)) => last_error = Some(error.to_string()),
                Err(_) => last_error = Some("connection timed out".to_owned()),
            }
        }
        Err(proxy_error(last_error.unwrap_or_else(|| {
            "host has no policy-allowed address".to_owned()
        })))
    }

    /// Returns whether this flow must terminate TLS to inspect HTTP requests.
    fn intercepts_tls_for_host(&self, host: &str) -> bool {
        matches!(&self.target, HttpProxyTarget::HostPort { .. })
            || self.policy.injects_secret_for_host(host)
    }

    /// Applies application-level admission and injects secrets for matching
    /// request rules.
    async fn apply_request_policy(
        &self,
        request: Request<Incoming>,
        host: &str,
        path: &str,
        method: &Method,
        workspace_name: &WorkspaceName,
        secrets: &SecretsService,
    ) -> ProxyResult<(Request<ProxyBody>, bool)> {
        let mut injection_rules = self
            .policy
            .secret_injection
            .iter()
            .filter(|rule| {
                NetworkPolicy::rule_matches(&rule.host, host)
                    && rule.matches_path(path)
                    && rule.methods.contains(method)
            })
            .collect::<Vec<_>>();
        if self.policy.injects_secret_for_host(host) && injection_rules.is_empty() {
            return Err(proxy_error(format!(
                "HTTP request {method} {path:?} is denied for secret-injection host {host:?}"
            )));
        }

        let inspect_graphql = injection_rules.iter().any(|rule| rule.graphql.is_some());
        let (mut request, graphql_error) = if inspect_graphql {
            inspect_graphql_request(request).await?
        } else {
            (box_incoming_request(request), None)
        };
        if let Some(error) = graphql_error {
            injection_rules.retain(|rule| rule.graphql.is_none());
            if injection_rules.is_empty() {
                return Err(error);
            }
        }

        let injected = self
            .inject_secrets(
                request.headers_mut(),
                &injection_rules,
                workspace_name,
                secrets,
            )
            .await?;
        Ok((request, injected))
    }

    async fn inject_secrets(
        &self,
        headers: &mut hyper::HeaderMap,
        rules: &[&SecretInjection],
        workspace_name: &WorkspaceName,
        secrets: &SecretsService,
    ) -> ProxyResult<bool> {
        let mut secrets_injected = false;
        for rule in rules {
            let secret = secrets
                .resolve_reference(
                    workspace_name,
                    self.policy.secrets.as_ref(),
                    &rule.reference,
                )
                .await
                .map_err(|error| {
                    proxy_error(format!("failed to resolve HTTP injection secret: {error}"))
                })?;
            if secret.len() > MAX_SECRET_BYTES || secret.contains(['\r', '\n']) {
                return Err(proxy_error(
                    "HTTP injection secret is not a valid header value",
                ));
            }
            for (header_name, value) in headers.iter_mut() {
                if forbidden_secret_header(header_name.as_str())
                    || rule
                        .header
                        .as_deref()
                        .is_some_and(|name| header_name.as_str() != name)
                {
                    continue;
                }
                let (injected_value, changed) =
                    inject_header_value(header_name, value, &rule.placeholder, &secret)?;
                *value = injected_value;
                secrets_injected |= changed;
            }
        }
        Ok(secrets_injected)
    }
}

async fn inspect_graphql_request(
    request: Request<Incoming>,
) -> ProxyResult<(Request<ProxyBody>, Option<Report<HttpProxyError>>)> {
    if let Err(error) = validate_graphql_headers(request.headers()) {
        return Ok((box_incoming_request(request), Some(error)));
    }
    let (parts, body) = request.into_parts();
    let body = Limited::new(body, MAX_GRAPHQL_REQUEST_BYTES)
        .collect()
        .await
        .map_err(|error| {
            proxy_error(format!(
                "failed to read bounded GraphQL request body: {error}"
            ))
        })?
        .to_bytes();
    let error = super::graphql::admit_queries_only(&body)
        .err()
        .map(|error| proxy_error(error.to_string()));
    let body = Full::new(body).map_err(|never| match never {}).boxed();
    Ok((Request::from_parts(parts, body), error))
}

fn validate_graphql_headers(headers: &hyper::HeaderMap) -> ProxyResult<()> {
    let mut content_types = headers.get_all(CONTENT_TYPE).iter();
    let content_type = content_types
        .next()
        .ok_or_else(|| proxy_error("GraphQL request Content-Type must be application/json"))?;
    if content_types.next().is_some()
        || !content_type.to_str().is_ok_and(|value| {
            value.split(';').next().is_some_and(|media_type| {
                media_type.trim().eq_ignore_ascii_case("application/json")
            })
        })
    {
        return Err(proxy_error(
            "GraphQL request Content-Type must be application/json",
        ));
    }
    if !headers.get_all(CONTENT_ENCODING).iter().all(|value| {
        value
            .to_str()
            .is_ok_and(|value| value.eq_ignore_ascii_case("identity"))
    }) {
        return Err(proxy_error(
            "compressed GraphQL request bodies are not supported",
        ));
    }
    Ok(())
}

fn box_incoming_request(request: Request<Incoming>) -> Request<ProxyBody> {
    request.map(|body| {
        body.map_err(|error| -> Box<dyn std::error::Error + Send + Sync> { Box::new(error) })
            .boxed()
    })
}

/// Records the bytes `rustls` consumes while parsing a bounded `ClientHello`.
struct ClientHelloRecordingIo<T> {
    inner: T,
    recorded: Vec<u8>,
    recording: bool,
}

impl<T> ClientHelloRecordingIo<T> {
    fn new(inner: T) -> Self {
        Self {
            inner,
            recorded: Vec::new(),
            recording: true,
        }
    }

    /// Stops capture and returns all bytes consumed during inspection.
    fn finish_recording(&mut self) -> Vec<u8> {
        self.recording = false;
        std::mem::take(&mut self.recorded)
    }

    /// Stops capture after `rustls` assumes ownership of the consumed bytes.
    fn discard_recording(&mut self) {
        self.recording = false;
        self.recorded.clear();
    }
}

impl<T: AsyncRead + Unpin> AsyncRead for ClientHelloRecordingIo<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let filled_before = buffer.filled().len();
        let result = Pin::new(&mut self.inner).poll_read(context, buffer);
        if matches!(result, Poll::Ready(Ok(()))) && self.recording {
            self.recorded
                .extend_from_slice(&buffer.filled()[filled_before..]);
        }
        result
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for ClientHelloRecordingIo<T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(context, bytes)
    }

    fn poll_flush(mut self: Pin<&mut Self>, context: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(context)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(context)
    }
}

async fn close_and_drain<T>(mut stream: SharedIo<T>, drain_timeout: Duration) -> ProxyResult<()>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    stream
        .shutdown()
        .await
        .map_err(|error| proxy_error(format!("failed to close pod-facing stream: {error}")))?;
    let drain = async {
        let mut buffer = [0_u8; 1024];
        loop {
            match stream.read(&mut buffer).await {
                Ok(0) => return Ok::<(), io::Error>(()),
                Ok(_) => {}
                Err(error) => return Err(error),
            }
        }
    };
    match timeout(drain_timeout, drain).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => debug!(%error, "failed to drain pod-facing HTTP stream"),
        Err(_) => debug!("timed out draining pod-facing HTTP stream"),
    }
    Ok(())
}

async fn send_request<T, U>(
    io: T,
    request: Request<ProxyBody>,
    pod_upgrade: Option<hyper::upgrade::OnUpgrade>,
    cleanup_io: SharedIo<U>,
    upgraded: Arc<AtomicBool>,
    upgrade_task: UpgradeTask,
    drain_timeout: Duration,
) -> ProxyResult<Response<ProxyBody>>
where
    T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    U: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut sender, connection) =
        client_http1::handshake(TokioIo::new(io))
            .await
            .map_err(|error| {
                proxy_error(format!("failed to start upstream HTTP connection: {error}"))
            })?;
    tokio::spawn(async move {
        if let Err(error) = connection.with_upgrades().await {
            debug!(%error, "upstream HTTP connection stopped");
        }
    });
    let mut response = sender.send_request(request).await.map_err(|error| {
        proxy_error(format!("failed to forward upstream HTTP request: {error}"))
    })?;
    if let Some(pod_upgrade) = pod_upgrade {
        if response.status() == StatusCode::SWITCHING_PROTOCOLS
            && requests_upgrade(response.headers())?
        {
            let upstream_upgrade = hyper::upgrade::on(&mut response);
            upgraded.store(true, Ordering::Release);
            let task = tokio::spawn(relay_upgrade(
                pod_upgrade,
                upstream_upgrade,
                cleanup_io,
                drain_timeout,
            ));
            *upgrade_task
                .lock()
                .map_err(|_| proxy_error("upgrade task lock is poisoned"))? = Some(task);
            strip_hop_by_hop(response.headers_mut(), true);
        } else {
            strip_hop_by_hop(response.headers_mut(), false);
        }
    } else {
        if response.status() == StatusCode::SWITCHING_PROTOCOLS {
            return Err(proxy_error(
                "upstream switched protocols without a valid upgrade request",
            ));
        }
        strip_hop_by_hop(response.headers_mut(), false);
    }
    debug!(status = %response.status(), "received upstream HTTP response");
    Ok(response.map(|body| {
        body.map_err(|error| -> Box<dyn std::error::Error + Send + Sync> { Box::new(error) })
            .boxed()
    }))
}

async fn relay_upgrade<T>(
    pod: hyper::upgrade::OnUpgrade,
    upstream: hyper::upgrade::OnUpgrade,
    cleanup_io: SharedIo<T>,
    drain_timeout: Duration,
) -> ProxyResult<()>
where
    T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let result = async {
        let pod = pod.await.map_err(|error| {
            proxy_error(format!("failed to upgrade pod HTTP connection: {error}"))
        })?;
        let upstream = upstream.await.map_err(|error| {
            proxy_error(format!(
                "failed to upgrade upstream HTTP connection: {error}"
            ))
        })?;
        let mut pod = TokioIo::new(pod);
        let mut upstream = TokioIo::new(upstream);
        copy_bidirectional(&mut pod, &mut upstream)
            .await
            .map_err(|error| {
                proxy_error(format!("failed to relay upgraded HTTP connection: {error}"))
            })?;
        Ok::<(), Report<HttpProxyError>>(())
    }
    .await;
    let relay_failed = result.is_err();
    if let Err(error) = &result {
        debug!(%error, "upgraded HTTP relay stopped");
    }
    if relay_failed && let Err(cleanup_error) = close_and_drain(cleanup_io, drain_timeout).await {
        debug!(%cleanup_error, "failed to close upgraded HTTP stream");
    }
    result
}

fn request_authority(request: &Request<Incoming>) -> ProxyResult<Authority> {
    let uri_authority = request.uri().authority().cloned();
    let header_authority = request
        .headers()
        .get(HOST)
        .map(|value| {
            let value = value
                .to_str()
                .map_err(|error| proxy_error(format!("HTTP Host is not text: {error}")))?;
            value
                .parse::<hyper::http::uri::Authority>()
                .map_err(|error| proxy_error(format!("HTTP Host is invalid: {error}")))
        })
        .transpose()?;
    if let (Some(uri), Some(header)) = (&uri_authority, &header_authority)
        && !uri.host().eq_ignore_ascii_case(header.host())
    {
        return Err(proxy_error(
            "request-target authority does not match HTTP Host",
        ));
    }
    uri_authority
        .or(header_authority)
        .ok_or_else(|| proxy_error("HTTP Host is required"))
}

/// Presents a mediated host-port request to its loopback service as localhost.
fn rewrite_host_port_request(
    headers: &mut hyper::HeaderMap,
    original_authority: &Authority,
) -> ProxyResult<()> {
    headers.insert(HOST, HeaderValue::from_static("localhost"));
    let mut values = headers.get_all(ORIGIN).iter();
    let Some(value) = values.next() else {
        return Ok(());
    };
    if values.next().is_some() {
        return Err(proxy_error("multiple HTTP Origin headers are not allowed"));
    }
    let text = value
        .to_str()
        .map_err(|error| proxy_error(format!("HTTP Origin is not text: {error}")))?;
    let origin: Uri = text
        .parse()
        .map_err(|error| proxy_error(format!("HTTP Origin is invalid: {error}")))?;
    let Some(authority) = origin.authority() else {
        return Ok(());
    };
    if !same_authority(authority, original_authority) {
        return Ok(());
    }
    let scheme = origin
        .scheme_str()
        .ok_or_else(|| proxy_error("HTTP Origin has no scheme"))?;
    headers.insert(
        ORIGIN,
        HeaderValue::from_str(&format!("{scheme}://localhost"))
            .map_err(|error| proxy_error(format!("rewritten HTTP Origin is invalid: {error}")))?,
    );
    Ok(())
}

fn same_authority(left: &Authority, right: &Authority) -> bool {
    left.host().eq_ignore_ascii_case(right.host()) && left.port_u16() == right.port_u16()
}

fn inject_header_value(
    name: &HeaderName,
    value: &HeaderValue,
    placeholder: &str,
    secret: &str,
) -> ProxyResult<(HeaderValue, bool)> {
    let text = value
        .to_str()
        .map_err(|error| proxy_error(format!("secret-bearing header is not text: {error}")))?;
    if name == hyper::header::AUTHORIZATION
        && let Some((scheme, encoded)) = text.split_once(' ')
        && scheme.eq_ignore_ascii_case("basic")
    {
        let decoded = BASE64.decode(encoded.trim()).map_err(|error| {
            proxy_error(format!(
                "Basic authorization credentials are not valid base64: {error}"
            ))
        })?;
        let (replaced, changed) =
            replace_bytes(&decoded, placeholder.as_bytes(), secret.as_bytes());
        if changed {
            return HeaderValue::from_str(&format!("Basic {}", BASE64.encode(replaced)))
                .map(|value| (value, true))
                .map_err(|error| {
                    proxy_error(format!(
                        "failed to encode injected Basic authorization header: {error}"
                    ))
                });
        }
    }
    let replaced = text.replace(placeholder, secret);
    let changed = replaced != text;
    HeaderValue::from_str(&replaced)
        .map(|value| (value, changed))
        .map_err(|error| proxy_error(format!("failed to encode injected HTTP header: {error}")))
}

fn replace_bytes(input: &[u8], needle: &[u8], replacement: &[u8]) -> (Vec<u8>, bool) {
    if needle.is_empty() {
        return (input.to_vec(), false);
    }
    let mut output = Vec::with_capacity(input.len());
    let mut remaining = input;
    let mut changed = false;
    while let Some(offset) = remaining
        .windows(needle.len())
        .position(|window| window == needle)
    {
        output.extend_from_slice(&remaining[..offset]);
        output.extend_from_slice(replacement);
        remaining = &remaining[offset + needle.len()..];
        changed = true;
    }
    output.extend_from_slice(remaining);
    (output, changed)
}

pub(crate) fn requests_upgrade(headers: &hyper::HeaderMap) -> ProxyResult<bool> {
    let has_upgrade = headers.contains_key(UPGRADE);
    let mut connection_upgrade = false;
    for value in headers.get_all(CONNECTION) {
        let value = value
            .to_str()
            .map_err(|error| proxy_error(format!("HTTP Connection is not text: {error}")))?;
        connection_upgrade |= value
            .split(',')
            .any(|token| token.trim().eq_ignore_ascii_case("upgrade"));
    }
    if has_upgrade != connection_upgrade {
        return Err(proxy_error(
            "HTTP upgrade requires both Connection: upgrade and Upgrade headers",
        ));
    }
    Ok(has_upgrade)
}

pub(crate) fn strip_hop_by_hop(headers: &mut hyper::HeaderMap, preserve_upgrade: bool) {
    let connection_headers = headers
        .get_all(CONNECTION)
        .iter()
        .filter_map(|value| match value.to_str() {
            Ok(value) => Some(value),
            Err(error) => {
                debug!(%error, "ignored non-text HTTP Connection header while sanitizing response");
                None
            }
        })
        .flat_map(|value| value.split(','))
        .filter_map(
            |name| match HeaderName::from_bytes(name.trim().as_bytes()) {
                Ok(name) => Some(name),
                Err(error) => {
                    debug!(%error, "ignored invalid HTTP Connection token while sanitizing response");
                    None
                }
            },
        )
        .collect::<Vec<_>>();
    for name in connection_headers {
        if !preserve_upgrade || name != UPGRADE {
            headers.remove(name);
        }
    }
    for name in [
        CONNECTION,
        HeaderName::from_static("proxy-connection"),
        HeaderName::from_static("keep-alive"),
        HeaderName::from_static("proxy-authenticate"),
        HeaderName::from_static("proxy-authorization"),
        HeaderName::from_static("te"),
        HeaderName::from_static("trailer"),
        HeaderName::from_static("transfer-encoding"),
        UPGRADE,
    ] {
        if !preserve_upgrade || (name != CONNECTION && name != UPGRADE) {
            headers.remove(name);
        }
    }
    if preserve_upgrade {
        headers.insert(CONNECTION, HeaderValue::from_static("upgrade"));
    } else {
        headers.insert(CONNECTION, HeaderValue::from_static("close"));
    }
}

fn error_response(error: &Report<HttpProxyError>) -> Response<ProxyBody> {
    let body = Full::new(Bytes::from(format!("Tascarrel network denied: {error}\n")))
        .map_err(|never| match never {})
        .boxed();
    Response::builder()
        .status(StatusCode::BAD_GATEWAY)
        .header(CONNECTION, "close")
        .body(body)
        .expect("static proxy error response is valid")
}

fn proxy_error(message: impl Into<String>) -> Report<HttpProxyError> {
    HttpProxyError::Failed(message.into()).report()
}

/// Returns whether a relay error reports that either TCP peer already closed.
fn is_closed_connection_error(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::BrokenPipe
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::NotConnected
    )
}

/// Builds the upstream TLS configuration used after HTTPS interception.
fn interception_client_config(authority: &WorkspaceAuthority) -> Arc<ClientConfig> {
    let loaded = rustls_native_certs::load_native_certs();
    let mut roots = RootCertStore::empty();
    for error in loaded.errors {
        debug!(%error, "could not load one native TLS root");
    }
    for certificate in loaded.certs {
        if let Err(error) = roots.add(certificate) {
            debug!(%error, "could not add one native TLS root");
        }
    }
    roots
        .add(authority.certificate_der())
        .expect("the parsed workspace CA is a valid trust anchor");
    let mut config = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Arc::new(config)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::num::NonZeroUsize;
    use std::os::unix::fs::PermissionsExt;

    use http_body_util::Empty;
    use hyper::header::AUTHORIZATION;
    use tascarrel_api::types::host::HostInstanceId;
    use tascarrel_api::types::network as api;
    use tokio::io::duplex;
    use tokio::net::TcpListener;
    use tokio_rustls::TlsAcceptor;

    use super::*;
    use crate::services::network::NetworkPolicy;
    use crate::services::network::activity::ActivityStream;
    use crate::services::network::activity::ActivitySubscription;
    use crate::services::secrets::SecretsServiceConfig;

    struct InterceptedTestRequest {
        method: Method,
        uri: &'static str,
        recorder: HttpRequestRecorder,
    }

    /// Exercises host-port authority rewriting and secret injection through
    /// one complete mediated HTTP request.
    #[tokio::test]
    async fn host_port_proxy_presents_localhost_and_injects_secrets() {
        let directory = tempfile::tempdir().unwrap();
        let workspaces = directory.path().join("workspaces");
        let workspace_name = WorkspaceName::new("proxy-test");
        let workspace = workspaces.join(workspace_name.as_str());
        fs::create_dir_all(&workspace).unwrap();
        let fake_sops = directory.path().join("fake-sops");
        fs::write(&fake_sops, "#!/bin/sh\nset -eu\ncat\n").unwrap();
        fs::set_permissions(&fake_sops, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(
            workspace.join("secrets.json"),
            r#"{"API_TOKEN":"super-secret"}"#,
        )
        .unwrap();
        fs::write(
            workspace.join("config.toml"),
            "[secrets.providers.project]\nkind = 'sops'\n\
             [network]\ndefault = 'deny'\n\
             [[network.secret-injection]]\nhost = 'host.tascarrel.internal'\n\
             paths = ['/v1/**']\nmethods = ['POST']\n\
             header = 'authorization'\nsecret = 'project.API_TOKEN'\n",
        )
        .unwrap();
        let policy = NetworkPolicy::load(&workspace.join("config.toml")).unwrap();
        let secrets =
            SecretsService::new(SecretsServiceConfig::new(&workspaces, fake_sops)).unwrap();

        let upstream = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let upstream_port = upstream.local_addr().unwrap().port();
        let upstream_task = tokio::spawn(async move {
            let (stream, _) = upstream.accept().await.unwrap();
            server_http1::Builder::new()
                .serve_connection(
                    TokioIo::new(stream),
                    service_fn(|request: Request<Incoming>| async move {
                        let host = request.headers()[HOST].to_str().unwrap();
                        let origin = request.headers()[ORIGIN].to_str().unwrap();
                        let authorization = request.headers()[AUTHORIZATION].to_str().unwrap();
                        Ok::<_, Infallible>(Response::new(Full::new(Bytes::from(format!(
                            "{host}\n{origin}\n{authorization}"
                        )))))
                    }),
                )
                .await
                .unwrap();
        });

        let proxy =
            HttpProxy::for_host_port(policy, None, Duration::from_secs(5), "127.0.0.1".to_owned());
        let (request_stream, mut request_activity) = test_request_stream();
        let (recorder, _) = test_request_recorder(&request_stream);
        let (client_io, proxy_io) = duplex(256 * 1024);
        let proxy_task = tokio::spawn(proxy.serve_http(
            proxy_io,
            upstream_port,
            workspace_name,
            secrets,
            recorder,
        ));
        let (mut sender, connection) = client_http1::handshake(TokioIo::new(client_io))
            .await
            .unwrap();
        let client_connection = tokio::spawn(connection);
        let response = sender
            .send_request(
                Request::builder()
                    .method(Method::POST)
                    .uri("/v1/chat")
                    .header(HOST, "host.tascarrel.internal:18080")
                    .header(ORIGIN, "http://host.tascarrel.internal:18080")
                    .header(AUTHORIZATION, "Bearer tascarrel-secret:api-token")
                    .body(Empty::<Bytes>::new())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.into_body().collect().await.unwrap().to_bytes(),
            "localhost\nhttp://localhost\nBearer super-secret"
        );
        drop(sender);

        timeout(Duration::from_secs(5), client_connection)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        timeout(Duration::from_secs(5), proxy_task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        timeout(Duration::from_secs(5), upstream_task)
            .await
            .unwrap()
            .unwrap();
        let batch = timeout(Duration::from_secs(5), request_activity.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(batch.entries.len(), 1);
        assert_eq!(
            batch.entries[0].host.as_deref(),
            Some("host.tascarrel.internal")
        );
        assert!(batch.entries[0].secrets_injected);
    }

    /// Exercises hostname admission and encrypted pass-through for an SNI that
    /// does not match the configured secret-injection rule.
    #[tokio::test]
    async fn https_proxy_relays_tls_without_a_workspace_authority() {
        install_crypto_provider();
        let directory = tempfile::tempdir().unwrap();
        let workspaces = directory.path().join("workspaces");
        let workspace_name = WorkspaceName::new("proxy-test");
        let workspace = workspaces.join(workspace_name.as_str());
        fs::create_dir_all(&workspace).unwrap();
        fs::write(
            workspace.join("config.toml"),
            "[secrets.providers.project]\nkind = 'sops'\n\
             [network]\ndefault = 'deny'\nallow-local = true\n\
             allow-hosts = ['localhost']\n\
             [[network.secret-injection]]\nhost = 'api.example'\n\
             methods = ['GET']\n\
             secret = 'project.API_TOKEN'\n",
        )
        .unwrap();
        let secrets = SecretsService::new(SecretsServiceConfig::new(
            &workspaces,
            directory.path().join("unused-sops"),
        ))
        .unwrap();
        let upstream_authority =
            WorkspaceAuthority::load_or_create(&directory.path().join("upstream"), "upstream-test")
                .unwrap();

        let upstream = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let upstream_port = upstream.local_addr().unwrap().port();
        let upstream_server_authority = Arc::clone(&upstream_authority);
        let upstream_task = tokio::spawn(async move {
            let (stream, _) = upstream.accept().await.unwrap();
            let tls = TlsAcceptor::from(
                upstream_server_authority
                    .server_config("localhost")
                    .unwrap(),
            )
            .accept(stream)
            .await
            .unwrap();
            server_http1::Builder::new()
                .serve_connection(
                    TokioIo::new(tls),
                    service_fn(|_request: Request<Incoming>| async move {
                        Ok::<_, Infallible>(Response::new(Full::new(Bytes::from_static(
                            b"encrypted-upstream",
                        ))))
                    }),
                )
                .await
                .unwrap();
        });

        let policy = NetworkPolicy::load(&workspace.join("config.toml")).unwrap();
        let proxy = HttpProxy::new(policy, None, Duration::from_secs(5));
        let (client_io, proxy_io) = duplex(256 * 1024);
        let proxy_task =
            spawn_unobserved_proxy(proxy, proxy_io, upstream_port, workspace_name, secrets);

        let mut roots = RootCertStore::empty();
        roots.add(upstream_authority.certificate_der()).unwrap();
        let mut client_config = ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        client_config.alpn_protocols = vec![b"http/1.1".to_vec()];
        let tls = TlsConnector::from(Arc::new(client_config))
            .connect(
                ServerName::try_from("localhost".to_owned()).unwrap(),
                client_io,
            )
            .await
            .unwrap();
        let (mut sender, connection) = client_http1::handshake(TokioIo::new(tls)).await.unwrap();
        let client_connection = tokio::spawn(connection);
        let response = sender
            .send_request(
                Request::builder()
                    .uri("/")
                    .header(HOST, "localhost")
                    .body(Empty::<Bytes>::new())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.into_body().collect().await.unwrap().to_bytes(),
            "encrypted-upstream"
        );
        drop(sender);

        timeout(Duration::from_secs(5), client_connection)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        timeout(Duration::from_secs(5), proxy_task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        timeout(Duration::from_secs(5), upstream_task)
            .await
            .unwrap()
            .unwrap();
    }

    /// Exercises HTTPS secret injection for one admitted request and host-side
    /// denial for requests outside the configured method or path.
    #[tokio::test]
    async fn https_proxy_enforces_secret_injection_request_scope() {
        install_crypto_provider();
        let directory = tempfile::tempdir().unwrap();
        let workspaces = directory.path().join("workspaces");
        let workspace_name = WorkspaceName::new("proxy-test");
        let workspace = workspaces.join(workspace_name.as_str());
        fs::create_dir_all(&workspace).unwrap();
        let fake_sops = directory.path().join("fake-sops");
        fs::write(&fake_sops, "#!/bin/sh\nset -eu\ncat\n").unwrap();
        fs::set_permissions(&fake_sops, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(
            workspace.join("secrets.json"),
            r#"{"API_TOKEN":"super-secret"}"#,
        )
        .unwrap();
        fs::write(
            workspace.join("config.toml"),
            "[secrets.providers.project]\nkind = 'sops'\n\
             [network]\nallow-local = true\n\
             [[network.secret-injection]]\nhost = 'localhost'\n\
             paths = ['/v1/models', '/v1/responses/*']\n\
             methods = ['GET']\n\
             header = 'authorization'\nsecret = 'project.API_TOKEN'\n",
        )
        .unwrap();
        let policy = NetworkPolicy::load(&workspace.join("config.toml")).unwrap();
        let secrets =
            SecretsService::new(SecretsServiceConfig::new(&workspaces, fake_sops)).unwrap();
        let authority =
            WorkspaceAuthority::load_or_create(&directory.path().join("authority"), "proxy-test")
                .unwrap();

        let upstream = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let upstream_port = upstream.local_addr().unwrap().port();
        let upstream_authority = Arc::clone(&authority);
        let upstream_task = tokio::spawn(async move {
            for _ in 0..2 {
                let (stream, _) = upstream.accept().await.unwrap();
                let tls = TlsAcceptor::from(upstream_authority.server_config("localhost").unwrap())
                    .accept(stream)
                    .await
                    .unwrap();
                server_http1::Builder::new()
                    .serve_connection(
                        TokioIo::new(tls),
                        service_fn(|request: Request<Incoming>| async move {
                            let authorization = request
                                .headers()
                                .get(AUTHORIZATION)
                                .and_then(|value| value.to_str().ok())
                                .unwrap_or_default()
                                .to_owned();
                            Ok::<_, Infallible>(Response::new(Full::new(Bytes::from(
                                authorization,
                            ))))
                        }),
                    )
                    .await
                    .unwrap();
            }
        });

        let proxy = HttpProxy::new(policy, Some(Arc::clone(&authority)), Duration::from_secs(5));
        let (request_stream, mut request_activity) = test_request_stream();
        let (exact_admitted_recorder, exact_admitted_flow_id) =
            test_request_recorder(&request_stream);
        let (status, body) = send_intercepted_https_request(
            proxy.clone(),
            &authority,
            upstream_port,
            workspace_name.clone(),
            secrets.clone(),
            InterceptedTestRequest {
                method: Method::GET,
                uri: "/v1/models?token=query-secret",
                recorder: exact_admitted_recorder,
            },
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "Bearer super-secret");

        let (glob_admitted_recorder, glob_admitted_flow_id) =
            test_request_recorder(&request_stream);
        let (status, body) = send_intercepted_https_request(
            proxy.clone(),
            &authority,
            upstream_port,
            workspace_name.clone(),
            secrets.clone(),
            InterceptedTestRequest {
                method: Method::GET,
                uri: "/v1/responses/42?token=query-secret",
                recorder: glob_admitted_recorder,
            },
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "Bearer super-secret");
        timeout(Duration::from_secs(5), upstream_task)
            .await
            .unwrap()
            .unwrap();

        let (method_denied_recorder, method_denied_flow_id) =
            test_request_recorder(&request_stream);
        let (status, body) = send_intercepted_https_request(
            proxy.clone(),
            &authority,
            upstream_port,
            workspace_name.clone(),
            secrets.clone(),
            InterceptedTestRequest {
                method: Method::POST,
                uri: "/v1/models?token=query-secret",
                recorder: method_denied_recorder,
            },
        )
        .await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert!(body.starts_with(
            b"Tascarrel network denied: HTTP proxy failed: HTTP request POST \"/v1/models\" is denied",
        ));

        let (path_denied_recorder, path_denied_flow_id) = test_request_recorder(&request_stream);
        let (status, body) = send_intercepted_https_request(
            proxy,
            &authority,
            upstream_port,
            workspace_name,
            secrets,
            InterceptedTestRequest {
                method: Method::GET,
                uri: "/v1/responses/team/42?token=query-secret",
                recorder: path_denied_recorder,
            },
        )
        .await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert!(body.starts_with(
            b"Tascarrel network denied: HTTP proxy failed: HTTP request GET \"/v1/responses/team/42\" is denied",
        ));

        let batch = timeout(Duration::from_secs(5), request_activity.recv())
            .await
            .unwrap()
            .unwrap();
        assert_request_activity(
            &batch.entries,
            &exact_admitted_flow_id,
            &glob_admitted_flow_id,
            &method_denied_flow_id,
            &path_denied_flow_id,
        );
    }

    /// Sends one request through an intercepting HTTPS proxy and returns its
    /// fully collected response.
    async fn send_intercepted_https_request(
        proxy: HttpProxy,
        authority: &Arc<WorkspaceAuthority>,
        upstream_port: u16,
        workspace_name: WorkspaceName,
        secrets: SecretsService,
        request: InterceptedTestRequest,
    ) -> (StatusCode, Bytes) {
        let (client_io, proxy_io) = duplex(256 * 1024);
        let proxy_task = spawn_proxy(
            proxy,
            proxy_io,
            upstream_port,
            workspace_name,
            secrets,
            request.recorder,
        );
        let mut roots = RootCertStore::empty();
        roots.add(authority.certificate_der()).unwrap();
        let mut client_config = ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        client_config.alpn_protocols = vec![b"http/1.1".to_vec()];
        let tls = TlsConnector::from(Arc::new(client_config))
            .connect(
                ServerName::try_from("localhost".to_owned()).unwrap(),
                client_io,
            )
            .await
            .unwrap();
        let (mut sender, connection) = client_http1::handshake(TokioIo::new(tls)).await.unwrap();
        let client_connection = tokio::spawn(connection);
        let request = Request::builder()
            .method(request.method)
            .uri(request.uri)
            .header(HOST, "localhost")
            .header(AUTHORIZATION, "Bearer tascarrel-secret:api-token")
            .body(Empty::<Bytes>::new())
            .unwrap();
        let response = sender.send_request(request).await.unwrap();
        let status = response.status();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        drop(sender);
        timeout(Duration::from_secs(5), client_connection)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        timeout(Duration::from_secs(5), proxy_task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        (status, body)
    }

    /// Starts one proxy task for an attributed HTTPS test flow.
    fn spawn_proxy<T>(
        proxy: HttpProxy,
        proxy_io: T,
        upstream_port: u16,
        workspace_name: WorkspaceName,
        secrets: SecretsService,
        recorder: HttpRequestRecorder,
    ) -> JoinHandle<ProxyResult<()>>
    where
        T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        tokio::spawn(proxy.serve_https(proxy_io, upstream_port, workspace_name, secrets, recorder))
    }

    /// Starts a proxy test flow whose request stream is intentionally ignored.
    fn spawn_unobserved_proxy<T>(
        proxy: HttpProxy,
        proxy_io: T,
        upstream_port: u16,
        workspace_name: WorkspaceName,
        secrets: SecretsService,
    ) -> JoinHandle<ProxyResult<()>>
    where
        T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (stream, _) = test_request_stream();
        let (recorder, _) = test_request_recorder(&stream);
        spawn_proxy(
            proxy,
            proxy_io,
            upstream_port,
            workspace_name,
            secrets,
            recorder,
        )
    }

    /// Verifies admitted and denied request summaries omit sensitive values.
    fn assert_request_activity(
        requests: &[api::MediatedHttpRequest],
        exact_admitted_flow_id: &api::TcpFlowId,
        glob_admitted_flow_id: &api::TcpFlowId,
        method_denied_flow_id: &api::TcpFlowId,
        path_denied_flow_id: &api::TcpFlowId,
    ) {
        assert_eq!(requests.len(), 4);
        let exact_admitted = &requests[0];
        assert_eq!(&exact_admitted.tcp_flow_id, exact_admitted_flow_id);
        assert_eq!(exact_admitted.host.as_deref(), Some("localhost"));
        assert_eq!(exact_admitted.method, "GET");
        assert_eq!(exact_admitted.path, "/v1/models");
        assert!(!exact_admitted.path_truncated);
        assert!(exact_admitted.secrets_injected);
        let glob_admitted = &requests[1];
        assert_eq!(&glob_admitted.tcp_flow_id, glob_admitted_flow_id);
        assert_eq!(glob_admitted.method, "GET");
        assert_eq!(glob_admitted.path, "/v1/responses/42");
        assert!(glob_admitted.secrets_injected);
        let method_denied = &requests[2];
        assert_eq!(&method_denied.tcp_flow_id, method_denied_flow_id);
        assert_eq!(method_denied.method, "POST");
        assert_eq!(method_denied.path, "/v1/models");
        assert!(!method_denied.secrets_injected);
        let path_denied = &requests[3];
        assert_eq!(&path_denied.tcp_flow_id, path_denied_flow_id);
        assert_eq!(path_denied.method, "GET");
        assert_eq!(path_denied.path, "/v1/responses/team/42");
        assert!(!path_denied.secrets_injected);
        let retained = format!("{requests:?}");
        assert!(!retained.contains("query-secret"));
        assert!(!retained.contains("super-secret"));
    }

    /// Creates one retained HTTP request stream and its initial subscriber.
    fn test_request_stream() -> (
        ActivityStream<api::MediatedHttpRequest>,
        ActivitySubscription<api::MediatedHttpRequest>,
    ) {
        let stream = ActivityStream::new(
            HostInstanceId::generate(),
            NonZeroUsize::new(8).unwrap(),
            NonZeroUsize::new(8).unwrap(),
        );
        let subscription = stream.subscribe(None);
        (stream, subscription)
    }

    /// Binds a recorder with a fresh flow identity to the shared test stream.
    fn test_request_recorder(
        stream: &ActivityStream<api::MediatedHttpRequest>,
    ) -> (HttpRequestRecorder, api::TcpFlowId) {
        let tcp_flow_id = api::TcpFlowId::generate();
        let recorder = HttpRequestRecorder::new(
            stream.clone(),
            tcp_flow_id.clone(),
            api::NetworkRequestSource::ImageBuild,
            NonZeroUsize::new(256).unwrap(),
        );
        (recorder, tcp_flow_id)
    }

    /// Installs the process-wide provider when another TLS test has not.
    fn install_crypto_provider() {
        if rustls::crypto::aws_lc_rs::default_provider()
            .install_default()
            .is_err()
        {
            tracing::debug!("rustls crypto provider was already installed");
        }
    }
}
