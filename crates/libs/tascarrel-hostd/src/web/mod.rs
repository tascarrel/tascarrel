//! Authenticated browser HTTP, static UI, routed services, and the Tascarrel
//! control `WebSocket`.
//!
//! [`WebServer`] serves the browser interface on hostd's loopback listener.
//! Browser-session middleware protects host APIs, while routed requests require
//! independent route-scoped credentials before reaching workspace services.

mod browser_auth;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::RwLock;
use std::time::Duration;

use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::Extension;
use axum::extract::Query;
use axum::extract::Request;
use axum::extract::State;
use axum::extract::ws::Message as WebSocketMessage;
use axum::extract::ws::WebSocket;
use axum::extract::ws::WebSocketUpgrade;
use axum::http::HeaderMap;
use axum::http::HeaderValue;
use axum::http::Method;
use axum::http::StatusCode;
use axum::http::Uri;
use axum::http::header;
use axum::middleware;
use axum::middleware::Next;
use axum::response::Html;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::response::Sse;
use axum::response::sse::Event;
use axum::response::sse::KeepAlive;
use axum::routing::get;
use axum::routing::post;
use futures_util::StreamExt;
use futures_util::stream;
use reportify::ErrorExt as _;
use serde::Deserialize;
use serde::Serialize;
use tascarrel_api::types::files::FileRoot;
use tascarrel_api::types::files::ShareFileRoot;
use tascarrel_api::types::host::ServerState;
use tascarrel_api::types::host::ServerStatus;
use tascarrel_api::types::protocol;
use tascarrel_protocol::ChatAttachmentReadRequest;
use tascarrel_protocol::ChatAttachmentReadResponse;
use tascarrel_protocol::ChatAttachmentUploadRequest;
use tascarrel_protocol::ChatAttachmentUploadResponse;
use tascarrel_protocol::DEFAULT_MAX_FRAME_LEN;
use tascarrel_protocol::FrameReader;
use tascarrel_protocol::Framed;
use tascarrel_protocol::MAX_CHAT_ATTACHMENT_BYTES;
use tascarrel_protocol::MAX_POD_FILE_WRITE_BYTES;
use tascarrel_protocol::MUX_CHAT_ATTACHMENT_READ_ENDPOINT;
use tascarrel_protocol::MUX_CHAT_ATTACHMENT_UPLOAD_ENDPOINT;
use tascarrel_protocol::MUX_POD_FILE_READ_ENDPOINT;
use tascarrel_protocol::MUX_POD_FILE_WRITE_ENDPOINT;
use tascarrel_protocol::PodFileReadRequest;
use tascarrel_protocol::PodFileReadResponse;
use tascarrel_protocol::PodFileWriteRejectionCode;
use tascarrel_protocol::PodFileWriteRequest;
use tascarrel_protocol::PodFileWriteResponse;
use tascarrel_protocol::WorkspaceName;
use tascarrel_protocol::control_plane;
use tokio::io::AsyncWriteExt as _;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_util::io::ReaderStream;
use tower::ServiceExt as _;
use tower_http::cors::AllowOrigin;
use tower_http::cors::CorsLayer;
use tower_http::services::ServeDir;
use tower_http::services::ServeFile;
use tracing::debug;
use tracing::info;
use tracing::warn;

use self::browser_auth::BROWSER_SESSION_COOKIE;
use self::browser_auth::ROUTE_AUTHORIZATION_PATH;
use self::browser_auth::TrustedFrontendBridge;
use self::browser_auth::authenticated_session;
use self::browser_auth::authentication_service_error;
use self::browser_auth::cookie_value;
use self::browser_auth::exact_request_origin;
use self::browser_auth::frontend_context;
use self::browser_auth::is_route_bridge_path;
use self::browser_auth::logout_browser;
use self::browser_auth::pair_browser;
use self::browser_auth::require_browser_session;
use self::browser_auth::route_authentication_required;
use self::browser_auth::route_authorization;
use self::browser_auth::trusted_bridge_uri;
use self::browser_auth::validate_browser_origin;
use crate::HostState;
use crate::control_plane::HostControlService;
use crate::services::auth::AuthService;
use crate::services::auth::AuthenticatedSession;
use crate::services::auth::HTTP_ROUTE_COOKIE_NAME;
use crate::startup::StartupReporter;

const FRONTEND_CORS_MAX_AGE: Duration = Duration::from_mins(10);
const UI_DOCUMENT_CACHE_CONTROL: HeaderValue = HeaderValue::from_static("no-store");
const UI_ASSET_CACHE_CONTROL: HeaderValue =
    HeaderValue::from_static("public, max-age=31536000, immutable");
const CHAT_ATTACHMENT_UPLOAD_PROOF: &str = "tascarrel-chat-attachment";
const POD_FILE_WRITE_PROOF: &str = "tascarrel-pod-file-write";
const STARTUP_PAGE: &str = include_str!("../startup.html");

pub(crate) struct WebServer {
    listener: TcpListener,
    state: WebState,
}

pub(crate) struct WebServerControl {
    retry: mpsc::Receiver<()>,
    state: WebState,
}

#[derive(Clone)]
struct WebState {
    auth: AuthService,
    public_origin: Option<String>,
    ready: Arc<RwLock<Option<ReadyWebState>>>,
    retry: mpsc::Sender<()>,
    status: StartupReporter,
    web_authority: SocketAddr,
}

#[derive(Clone)]
struct ReadyWebState {
    host: HostState,
    control: HostControlService,
    ui_root: Option<PathBuf>,
}

impl ReadyWebState {
    fn workspace_service(&self) -> &crate::WorkspaceService {
        self.host.workspaces()
    }

    fn network_service(&self) -> &crate::NetworkService {
        self.host.network()
    }
}

impl WebState {
    fn ready(&self) -> Option<ReadyWebState> {
        match self.ready.read() {
            Ok(ready) => ready.clone(),
            Err(poisoned) => {
                warn!(error = %poisoned, "web readiness lock was poisoned while reading");
                poisoned.into_inner().clone()
            }
        }
    }

    fn set_ready(&self, ready: ReadyWebState) {
        match self.ready.write() {
            Ok(mut current) => *current = Some(ready),
            Err(poisoned) => {
                warn!(error = %poisoned, "web readiness lock was poisoned while writing");
                *poisoned.into_inner() = Some(ready);
            }
        }
    }

    fn require_ready(&self) -> Result<ReadyWebState, ApiError> {
        self.ready().ok_or_else(|| {
            ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "server-starting",
                "the Tascarrel server is not ready",
            )
        })
    }
}

impl WebServer {
    /// Serves the embedded startup document, status API, and ready application.
    ///
    /// # Errors
    ///
    /// Returns an error if the Axum server stops unexpectedly.
    #[tracing::instrument(name = "tascarrel_host.web.serve", level = "info", skip_all, err)]
    pub(crate) async fn serve(self) -> anyhow::Result<()> {
        info!(address = %self.state.web_authority, "Tascarrel bootstrap web interface ready");
        axum::serve(self.listener, router(self.state)).await?;
        Ok(())
    }
}

impl WebServerControl {
    /// Publishes the initialized host services to ready-only HTTP routes.
    pub(crate) fn make_ready(
        &self,
        host: HostState,
        control: HostControlService,
        ui_root: Option<PathBuf>,
    ) {
        self.state.set_ready(ReadyWebState {
            host,
            control,
            ui_root,
        });
    }

    /// Waits for the next startup retry requested through the bootstrap page.
    pub(crate) async fn retry_requested(&mut self) {
        if self.retry.recv().await.is_none() {
            std::future::pending::<()>().await;
        }
    }
}

/// Binds the bootstrap listener before payload preparation or host checks.
///
/// # Errors
///
/// Returns an error when the configured loopback listener cannot be bound.
#[tracing::instrument(name = "tascarrel_host.web.bind", level = "info", skip(status), err)]
pub(crate) async fn bind(
    address: SocketAddr,
    status: StartupReporter,
    auth: AuthService,
    public_origin: Option<String>,
) -> anyhow::Result<(WebServer, WebServerControl)> {
    let listener = TcpListener::bind(address).await?;
    let web_authority = listener.local_addr()?;
    let (retry, retry_requests) = mpsc::channel(1);
    let state = WebState {
        auth,
        public_origin,
        ready: Arc::new(RwLock::new(None)),
        retry,
        status,
        web_authority,
    };
    Ok((
        WebServer {
            listener,
            state: state.clone(),
        },
        WebServerControl {
            retry: retry_requests,
            state,
        },
    ))
}

fn router(state: WebState) -> Router {
    let network_state = state.clone();
    let cors = frontend_cors(&state);
    let protected = Router::new()
        .route("/api/v1/server/status", get(server_status))
        .route("/api/v1/server/status/events", get(server_status_events))
        .route("/api/v1/server/retry", post(retry_startup))
        .route("/api/v1/auth/session", get(authenticated_session))
        .route("/api/v1/auth/context", get(frontend_context))
        .route("/api/v1/auth/logout", post(logout_browser))
        .route("/api/v1/control", get(control_upgrade))
        .route(
            "/api/v1/chat/upload-attachment",
            post(upload_chat_attachment),
        )
        .route("/api/v1/chat/attachment", get(read_chat_attachment))
        .route("/api/v1/files/raw", get(raw_file).put(write_raw_file))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_browser_session,
        ));
    Router::new()
        .route("/api/health", get(health))
        .route("/api/v1/auth/pair", post(pair_browser))
        .merge(protected)
        .fallback(serve_ui)
        .with_state(state)
        .layer(middleware::from_fn(ui_cache_headers))
        .layer(cors)
        .layer(middleware::from_fn_with_state(
            network_state,
            forward_network_request,
        ))
}

async fn health() -> StatusCode {
    StatusCode::NO_CONTENT
}

async fn serve_ui(State(state): State<WebState>, request: Request) -> Response {
    let startup_requested = request.uri().path() == "/startup";
    let Some(ready) = state.ready().filter(|_| !startup_requested) else {
        return startup_page();
    };
    let Some(ui_root) = ready.ui_root else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let index = ui_root.join("index.html");
    match ServeDir::new(ui_root)
        .not_found_service(ServeFile::new(index))
        .oneshot(request)
        .await
    {
        Ok(response) => response.into_response(),
        Err(error) => match error {},
    }
}

fn startup_page() -> Response {
    let mut response = Html(STARTUP_PAGE).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, UI_DOCUMENT_CACHE_CONTROL);
    response.headers_mut().insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'none'; connect-src 'self'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; base-uri 'none'; form-action 'none'",
        ),
    );
    response.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response
}

async fn server_status(State(state): State<WebState>) -> Json<ServerStatus> {
    Json(state.status.current())
}

async fn server_status_events(
    State(state): State<WebState>,
) -> Sse<impl futures_util::Stream<Item = Result<Event, axum::Error>>> {
    let stream = stream::unfold(
        (state.status.subscribe(), true),
        |(mut status, initial)| async move {
            if !initial && status.changed().await.is_err() {
                return None;
            }
            let snapshot = status.borrow_and_update().clone();
            Some((Event::default().json_data(snapshot), (status, false)))
        },
    );
    Sse::new(stream).keep_alive(KeepAlive::default())
}

async fn retry_startup(
    State(state): State<WebState>,
    headers: HeaderMap,
) -> Result<StatusCode, ApiError> {
    validate_browser_origin(&state, &headers)?;
    let token = cookie_value(&headers, BROWSER_SESSION_COOKIE).ok_or_else(|| {
        ApiError::unauthorized(
            "authentication-required",
            "a paired Tascarrel browser session is required",
        )
    })?;
    state
        .auth
        .authenticate_browser(&token)
        .await
        .map_err(|_| ApiError::unauthorized("invalid-session", "browser session is invalid"))?;
    if !matches!(state.status.current().state, ServerState::Failed(_)) {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "server-not-failed",
            "the Tascarrel server is not waiting for a startup retry",
        ));
    }
    match state.retry.try_send(()) {
        Ok(()) | Err(mpsc::error::TrySendError::Full(())) => Ok(StatusCode::ACCEPTED),
        Err(mpsc::error::TrySendError::Closed(())) => Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "retry-unavailable",
            "the Tascarrel startup task is unavailable",
        )),
    }
}

/// Builds the browser API policy for canonical and explicitly trusted origins.
fn frontend_cors(state: &WebState) -> CorsLayer {
    let state = state.clone();
    let web_authority = state.web_authority;
    CorsLayer::new()
        .allow_origin(AllowOrigin::predicate(move |origin, _request| {
            state.ready().is_some_and(|ready| {
                is_allowed_frontend_origin(origin, web_authority, ready.network_service())
            })
        }))
        .allow_methods([Method::GET, Method::POST, Method::PUT])
        .allow_headers([
            header::CONTENT_TYPE,
            header::IF_MATCH,
            header::HeaderName::from_static("x-tascarrel-request"),
        ])
        .expose_headers([
            header::ETAG,
            header::HeaderName::from_static("x-tascarrel-file-writable"),
        ])
        .max_age(FRONTEND_CORS_MAX_AGE)
}

/// Returns whether a browser origin may access the Tascarrel HTTP API.
fn is_allowed_frontend_origin(
    origin: &HeaderValue,
    web_authority: SocketAddr,
    network: &crate::NetworkService,
) -> bool {
    let Some(authority) = origin
        .to_str()
        .ok()
        .and_then(|origin| origin.parse::<Uri>().ok())
        .filter(|origin| matches!(origin.scheme_str(), Some("http" | "https")))
        .and_then(|origin| origin.authority().cloned())
    else {
        return false;
    };
    network.is_frontend_authority(&authority, web_authority)
        || network.is_trusted_tascarrel_frontend_authority(&authority, web_authority)
}

async fn forward_network_request(
    State(state): State<WebState>,
    mut request: Request,
    next: Next,
) -> Response {
    let Some(ready) = state.ready() else {
        return next.run(request).await;
    };
    let route = match ready
        .network_service()
        .resolve_http_route(request.headers())
    {
        Ok(Some(route)) => route,
        Ok(None) => return next.run(request).await,
        Err(error) => return network_proxy_error(&error),
    };
    if request.uri().path() == ROUTE_AUTHORIZATION_PATH {
        return route_authorization(&state, request, &route).await;
    }
    let Some(token) = cookie_value(request.headers(), HTTP_ROUTE_COOKIE_NAME) else {
        return route_authentication_required();
    };
    let session = match state
        .auth
        .authenticate_route(&token, route.id(), route.hostname())
        .await
    {
        Ok(session) => session,
        Err(error)
            if error.error() == &crate::services::auth::AuthServiceError::InvalidRouteGrant =>
        {
            debug!(%error, "HTTP route grant authentication failed");
            return route_authentication_required();
        }
        Err(error) => return authentication_service_error(&error).into_response(),
    };
    if is_route_bridge_path(request.uri().path()) {
        if !route.is_trusted_tascarrel_frontend() {
            return StatusCode::NOT_FOUND.into_response();
        }
        let Some(rewritten) = trusted_bridge_uri(request.uri()) else {
            return StatusCode::NOT_FOUND.into_response();
        };
        *request.uri_mut() = rewritten;
        request.extensions_mut().insert(session);
        request.extensions_mut().insert(TrustedFrontendBridge);
        return next.run(request).await;
    }
    match ready
        .network_service()
        .forward_http(request, route, ready.workspace_service())
        .await
    {
        Ok(response) => response,
        Err(error) => network_proxy_error(&error),
    }
}

fn network_proxy_error(error: &reportify::Report<crate::NetworkProxyError>) -> Response {
    debug!(%error, "routed HTTP request failed");
    let mut response = (
        error.error().status(),
        format!("Tascarrel network proxy: {error}\n"),
    )
        .into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response
}

async fn ui_cache_headers(request: Request, next: Next) -> Response {
    let path = request.uri().path().to_owned();
    let mut response = next.run(request).await;
    if !response.headers().contains_key(header::CACHE_CONTROL) {
        let cache_control = ui_cache_control(&path, response.status(), response.headers());
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, cache_control);
    }
    response
}

fn ui_cache_control(path: &str, status: StatusCode, headers: &HeaderMap) -> HeaderValue {
    let html = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("text/html"));
    if status.is_success() && is_hashed_ui_asset(path) && !html {
        UI_ASSET_CACHE_CONTROL
    } else {
        UI_DOCUMENT_CACHE_CONTROL
    }
}

fn is_hashed_ui_asset(path: &str) -> bool {
    let Some(filename) = path.strip_prefix("/assets/") else {
        return false;
    };
    let Some((stem, extension)) = filename.rsplit_once('.') else {
        return false;
    };
    if extension.is_empty() || stem.len() < 9 {
        return false;
    }
    let hash_start = stem.len() - 8;
    if !stem.is_char_boundary(hash_start) {
        return false;
    }
    let (prefix, hash) = stem.split_at(hash_start);
    prefix.ends_with('-')
        && hash
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct UploadChatAttachmentQuery {
    workspace: String,
    name: String,
}

#[allow(clippy::too_many_lines)] // Upload streaming keeps peer cancellation and response ordering together.
/// Uploads one bounded chat attachment through the guest data plane.
#[tracing::instrument(
    level = "debug",
    skip(state, input, headers, body),
    fields(workspace = %input.workspace),
    err(Debug)
)]
async fn upload_chat_attachment(
    State(state): State<WebState>,
    Query(input): Query<UploadChatAttachmentQuery>,
    headers: HeaderMap,
    body: Body,
) -> Result<Json<tascarrel_api::types::chats::ChatPromptAttachment>, ApiError> {
    let state = state.require_ready()?;
    if headers
        .get("x-tascarrel-request")
        .and_then(|value| value.to_str().ok())
        != Some(CHAT_ATTACHMENT_UPLOAD_PROOF)
    {
        return Err(ApiError::forbidden(
            "missing-request-proof",
            "missing chat attachment upload request proof",
        ));
    }
    if headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .is_some_and(|length| length > MAX_CHAT_ATTACHMENT_BYTES)
    {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "attachment-too-large",
            format!("attachment exceeds the {MAX_CHAT_ATTACHMENT_BYTES}-byte limit"),
        ));
    }
    let workspace = WorkspaceName::new(input.workspace)
        .map_err(|error| ApiError::bad_request("invalid-workspace", error.to_string()))?;
    let media_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("application/octet-stream")
        .to_owned();
    let mux = state
        .workspace_service()
        .connect(workspace)
        .await
        .map_err(|error| {
            ApiError::new(
                StatusCode::BAD_GATEWAY,
                "workspace-unavailable",
                error.to_string(),
            )
        })?;
    let channel = mux
        .open(MUX_CHAT_ATTACHMENT_UPLOAD_ENDPOINT)
        .await
        .map_err(|error| {
            ApiError::new(
                StatusCode::BAD_GATEWAY,
                "attachment-upload-unavailable",
                format!("open workspace attachment upload: {error}"),
            )
        })?;
    let mut framed = Framed::new(channel);
    framed
        .write(&ChatAttachmentUploadRequest {
            name: input.name,
            media_type,
        })
        .await
        .map_err(|error| {
            ApiError::new(
                StatusCode::BAD_GATEWAY,
                "attachment-upload-failed",
                format!("send attachment metadata: {error}"),
            )
        })?;
    let (reader, mut writer) = tokio::io::split(framed.into_inner());
    let mut response = FrameReader::new(reader);
    let upload = async move {
        let mut chunks = body.into_data_stream();
        while let Some(chunk) = chunks.next().await {
            let chunk = chunk.map_err(|error| error.to_string())?;
            writer
                .write_all(&chunk)
                .await
                .map_err(|error| error.to_string())?;
        }
        writer.shutdown().await.map_err(|error| error.to_string())
    };
    let receive = response.read::<ChatAttachmentUploadResponse>();
    tokio::pin!(upload);
    tokio::pin!(receive);
    let result = tokio::select! {
        result = &mut receive => result,
        upload_result = &mut upload => {
            upload_result.map_err(|error| {
                ApiError::new(
                    StatusCode::BAD_GATEWAY,
                    "attachment-upload-failed",
                    format!("stream attachment content: {error}"),
                )
            })?;
            receive.await
        }
    }
    .map_err(|error| {
        ApiError::new(
            StatusCode::BAD_GATEWAY,
            "attachment-upload-failed",
            format!("read attachment result: {error}"),
        )
    })?
    .ok_or_else(|| {
        ApiError::new(
            StatusCode::BAD_GATEWAY,
            "attachment-upload-failed",
            "workspace attachment upload closed without a result",
        )
    })?;
    match result {
        ChatAttachmentUploadResponse::Uploaded { attachment } => Ok(Json(attachment)),
        ChatAttachmentUploadResponse::Rejected { message, .. } => {
            Err(ApiError::bad_request("attachment-rejected", message))
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ReadChatAttachmentQuery {
    workspace: String,
    attachment_id: String,
}

#[allow(clippy::too_many_lines)] // Response validation and security headers form one delivery boundary.
/// Streams one stored chat attachment with safe response headers.
#[tracing::instrument(
    level = "debug",
    skip(state, input),
    fields(workspace = %input.workspace),
    err(Debug)
)]
async fn read_chat_attachment(
    State(state): State<WebState>,
    Query(input): Query<ReadChatAttachmentQuery>,
) -> Result<Response, ApiError> {
    let state = state.require_ready()?;
    let workspace = WorkspaceName::new(input.workspace)
        .map_err(|error| ApiError::bad_request("invalid-workspace", error.to_string()))?;
    let attachment_id = input
        .attachment_id
        .parse::<tascarrel_api::ids::ChatAttachmentId>()
        .map_err(|error| ApiError::bad_request("invalid-attachment", error.to_string()))?;
    let mux = state
        .workspace_service()
        .connect(workspace)
        .await
        .map_err(|error| {
            ApiError::new(
                StatusCode::BAD_GATEWAY,
                "workspace-unavailable",
                error.to_string(),
            )
        })?;
    let channel = mux
        .open(MUX_CHAT_ATTACHMENT_READ_ENDPOINT)
        .await
        .map_err(|error| {
            ApiError::new(
                StatusCode::BAD_GATEWAY,
                "attachment-read-unavailable",
                format!("open workspace attachment read: {error}"),
            )
        })?;
    let mut framed = Framed::new(channel);
    framed
        .write(&ChatAttachmentReadRequest { attachment_id })
        .await
        .map_err(|error| {
            ApiError::new(
                StatusCode::BAD_GATEWAY,
                "attachment-read-failed",
                format!("send attachment request: {error}"),
            )
        })?;
    let result = framed
        .read::<ChatAttachmentReadResponse>()
        .await
        .map_err(|error| {
            ApiError::new(
                StatusCode::BAD_GATEWAY,
                "attachment-read-failed",
                format!("read attachment response: {error}"),
            )
        })?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::BAD_GATEWAY,
                "attachment-read-failed",
                "workspace attachment read closed without a response",
            )
        })?;
    let attachment = match result {
        ChatAttachmentReadResponse::Found { attachment } => attachment,
        ChatAttachmentReadResponse::Rejected { code, message } => {
            return Err(ApiError::new(
                if code == "not_found" {
                    StatusCode::NOT_FOUND
                } else {
                    StatusCode::BAD_REQUEST
                },
                code,
                message,
            ));
        }
    };
    let mut response = Response::new(Body::from_stream(ReaderStream::new(framed.into_inner())));
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(attachment.media_type.as_ref()).map_err(|_| {
            ApiError::new(
                StatusCode::BAD_GATEWAY,
                "invalid-attachment-metadata",
                "workspace returned an invalid attachment media type",
            )
        })?,
    );
    headers.insert(
        header::CONTENT_LENGTH,
        HeaderValue::from_str(&attachment.size.to_string()).expect("u64 is a valid header value"),
    );
    headers.insert(
        header::CONTENT_DISPOSITION,
        if chat_attachment_can_render_inline(attachment.media_type.as_ref()) {
            HeaderValue::from_static("inline")
        } else {
            HeaderValue::from_static("attachment")
        },
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=31536000, immutable"),
    );
    headers.insert(
        header::ETAG,
        HeaderValue::from_str(&format!("\"{}\"", attachment.digest)).map_err(|_| {
            ApiError::new(
                StatusCode::BAD_GATEWAY,
                "invalid-attachment-metadata",
                "workspace returned an invalid attachment digest",
            )
        })?,
    );
    if attachment.media_type.as_ref() == "image/svg+xml" {
        headers.insert(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static("sandbox; default-src 'none'; style-src 'unsafe-inline'"),
        );
    }
    Ok(response)
}

/// Restricts inline responses to passive formats used by the attachment
/// preview components.
fn chat_attachment_can_render_inline(media_type: &str) -> bool {
    matches!(
        media_type,
        "application/pdf"
            | "image/avif"
            | "image/bmp"
            | "image/gif"
            | "image/jpeg"
            | "image/png"
            | "image/webp"
            | "image/x-icon"
    )
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawFileQuery {
    workspace: String,
    pod_id: String,
    share: Option<String>,
    path: String,
    #[serde(default)]
    download: bool,
}

#[tracing::instrument(
    level = "debug",
    skip_all,
    fields(workspace = %input.workspace, pod_id = %input.pod_id, share = ?input.share, path = %input.path)
)]
async fn raw_file(
    State(state): State<WebState>,
    Query(input): Query<RawFileQuery>,
) -> Result<Response, ApiError> {
    let state = state.require_ready()?;
    let workspace = WorkspaceName::new(input.workspace)
        .map_err(|error| ApiError::bad_request("invalid-workspace", error.to_string()))?;
    let pod_id = input
        .pod_id
        .parse::<tascarrel_api::ids::PodId>()
        .map_err(|error| ApiError::bad_request("invalid-pod", error.to_string()))?;
    let mux = state
        .workspace_service()
        .connect(workspace)
        .await
        .map_err(|error| {
            ApiError::new(
                StatusCode::BAD_GATEWAY,
                "workspace-unavailable",
                error.to_string(),
            )
        })?;
    let channel = mux
        .open(MUX_POD_FILE_READ_ENDPOINT)
        .await
        .map_err(|error| {
            ApiError::new(
                StatusCode::BAD_GATEWAY,
                "file-read-unavailable",
                format!("failed to open pod file read: {error}"),
            )
        })?;
    let mut framed = Framed::new(channel);
    framed
        .write(&PodFileReadRequest {
            pod_id,
            root: raw_file_root(input.share.as_deref()),
            path: tascarrel_api::types::files::FilePath::new(input.path.clone()),
        })
        .await
        .map_err(|error| {
            ApiError::new(
                StatusCode::BAD_GATEWAY,
                "file-read-failed",
                format!("failed to send pod file request: {error}"),
            )
        })?;
    framed.get_mut().shutdown().await.map_err(|error| {
        ApiError::new(
            StatusCode::BAD_GATEWAY,
            "file-read-failed",
            format!("failed to finish pod file request: {error}"),
        )
    })?;
    let result = framed
        .read::<PodFileReadResponse>()
        .await
        .map_err(|error| {
            ApiError::new(
                StatusCode::BAD_GATEWAY,
                "file-read-failed",
                format!("failed to read pod file response: {error}"),
            )
        })?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::BAD_GATEWAY,
                "file-read-failed",
                "pod file read closed without a response",
            )
        })?;
    let (size, writable) = match result {
        PodFileReadResponse::Found { size, writable } => (size, writable),
        PodFileReadResponse::Rejected { code, message } => {
            return Err(pod_file_rejection(code, message));
        }
    };
    let body = Body::from_stream(ReaderStream::new(framed.into_inner()));
    Ok(raw_file_response(
        &input.path,
        input.download,
        size,
        writable,
        body,
    ))
}

/// Builds the uncached response for one streamed pod file.
fn raw_file_response(
    path: &str,
    download: bool,
    size: u64,
    writable: bool,
    body: Body,
) -> Response {
    let mut response = Response::new(body);
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(content_type(path)),
    );
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(size));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::HeaderName::from_static("x-tascarrel-file-writable"),
        HeaderValue::from_static(if writable { "true" } else { "false" }),
    );
    if path.to_ascii_lowercase().ends_with(".svg") {
        headers.insert(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static("sandbox; default-src 'none'; style-src 'unsafe-inline'"),
        );
    }
    if download {
        headers.insert(
            header::CONTENT_DISPOSITION,
            HeaderValue::from_static("attachment"),
        );
    }
    response
}

/// Replaces one pod text file through the guest data plane.
#[tracing::instrument(
    level = "debug",
    skip(state, input, headers, body),
    fields(workspace = %input.workspace, pod_id = %input.pod_id, share = ?input.share, path = %input.path),
    err(Debug)
)]
async fn write_raw_file(
    State(state): State<WebState>,
    Query(input): Query<RawFileQuery>,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, ApiError> {
    let state = state.require_ready()?;
    let expected_revision = validate_pod_file_write_headers(&headers)?;
    let workspace = WorkspaceName::new(input.workspace)
        .map_err(|error| ApiError::bad_request("invalid-workspace", error.to_string()))?;
    let pod_id = input
        .pod_id
        .parse::<tascarrel_api::ids::PodId>()
        .map_err(|error| ApiError::bad_request("invalid-pod", error.to_string()))?;
    let mux = state
        .workspace_service()
        .connect(workspace)
        .await
        .map_err(|error| {
            ApiError::new(
                StatusCode::BAD_GATEWAY,
                "workspace-unavailable",
                error.to_string(),
            )
        })?;
    let channel = mux
        .open(MUX_POD_FILE_WRITE_ENDPOINT)
        .await
        .map_err(|error| {
            ApiError::new(
                StatusCode::BAD_GATEWAY,
                "file-write-unavailable",
                format!("failed to open pod file write: {error}"),
            )
        })?;
    let mut framed = Framed::new(channel);
    framed
        .write(&PodFileWriteRequest {
            pod_id,
            root: raw_file_root(input.share.as_deref()),
            path: tascarrel_api::types::files::FilePath::new(input.path),
            expected_revision,
        })
        .await
        .map_err(|error| {
            ApiError::new(
                StatusCode::BAD_GATEWAY,
                "file-write-failed",
                format!("failed to send pod file write request: {error}"),
            )
        })?;
    let result = stream_pod_file_write(framed.into_inner(), body).await?;
    pod_file_write_response(result)
}

/// Validates the browser proof, size declaration, and expected revision.
fn validate_pod_file_write_headers(headers: &HeaderMap) -> Result<String, ApiError> {
    if headers
        .get("x-tascarrel-request")
        .and_then(|value| value.to_str().ok())
        != Some(POD_FILE_WRITE_PROOF)
    {
        return Err(ApiError::forbidden(
            "missing-request-proof",
            "missing pod file write request proof",
        ));
    }
    if headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .is_some_and(|length| length > MAX_POD_FILE_WRITE_BYTES)
    {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "file-too-large",
            format!("replacement exceeds the {MAX_POD_FILE_WRITE_BYTES}-byte editor limit"),
        ));
    }
    file_revision(headers)
}

/// Streams replacement bytes while observing an early guest rejection.
async fn stream_pod_file_write(
    channel: tascarrel_mux::Channel,
    body: Body,
) -> Result<PodFileWriteResponse, ApiError> {
    let (reader, mut writer) = tokio::io::split(channel);
    let mut response = FrameReader::new(reader);
    let upload = async move {
        let mut chunks = body.into_data_stream();
        while let Some(chunk) = chunks.next().await {
            let chunk = chunk.map_err(|error| error.to_string())?;
            writer
                .write_all(&chunk)
                .await
                .map_err(|error| error.to_string())?;
        }
        writer.shutdown().await.map_err(|error| error.to_string())
    };
    let receive = response.read::<PodFileWriteResponse>();
    tokio::pin!(upload);
    tokio::pin!(receive);
    tokio::select! {
        result = &mut receive => result,
        upload_result = &mut upload => {
            upload_result.map_err(|error| {
                ApiError::new(
                    StatusCode::BAD_GATEWAY,
                    "file-write-failed",
                    format!("failed to stream pod file replacement: {error}"),
                )
            })?;
            receive.await
        }
    }
    .map_err(|error| {
        ApiError::new(
            StatusCode::BAD_GATEWAY,
            "file-write-failed",
            format!("failed to read pod file write result: {error}"),
        )
    })?
    .ok_or_else(|| {
        ApiError::new(
            StatusCode::BAD_GATEWAY,
            "file-write-failed",
            "pod file write closed without a result",
        )
    })
}

/// Maps a guest replacement result into the browser HTTP contract.
fn pod_file_write_response(result: PodFileWriteResponse) -> Result<Response, ApiError> {
    match result {
        PodFileWriteResponse::Written { revision } => {
            let mut response = StatusCode::NO_CONTENT.into_response();
            response.headers_mut().insert(
                header::ETAG,
                HeaderValue::from_str(&format!("\"{revision}\"")).map_err(|_| {
                    ApiError::new(
                        StatusCode::BAD_GATEWAY,
                        "invalid-file-revision",
                        "workspace returned an invalid file revision",
                    )
                })?,
            );
            Ok(response)
        }
        PodFileWriteResponse::Conflict => Err(ApiError::new(
            StatusCode::PRECONDITION_FAILED,
            "file-conflict",
            "file changed since it was read",
        )),
        PodFileWriteResponse::Rejected { code, message } => {
            let status = match code {
                PodFileWriteRejectionCode::InvalidRequest => StatusCode::BAD_REQUEST,
                PodFileWriteRejectionCode::ReadOnly => StatusCode::FORBIDDEN,
                PodFileWriteRejectionCode::TooLarge => StatusCode::PAYLOAD_TOO_LARGE,
                PodFileWriteRejectionCode::Unavailable | PodFileWriteRejectionCode::Internal => {
                    StatusCode::BAD_GATEWAY
                }
            };
            Err(ApiError::new(status, code.as_str(), message))
        }
    }
}

/// Extracts one strong lowercase SHA-256 entity tag from `If-Match`.
fn file_revision(headers: &HeaderMap) -> Result<String, ApiError> {
    let revision = headers
        .get(header::IF_MATCH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix('"'))
        .and_then(|value| value.strip_suffix('"'))
        .filter(|value| {
            value.len() == 64
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        })
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::PRECONDITION_REQUIRED,
                "missing-file-revision",
                "a valid If-Match file revision is required",
            )
        })?;
    Ok(revision.to_owned())
}

/// Converts the optional HTTP share selector into the Files API root.
fn raw_file_root(share: Option<&str>) -> FileRoot {
    share.map_or(FileRoot::Workspace, |name| {
        FileRoot::Share(ShareFileRoot {
            name: name.to_owned().into(),
        })
    })
}

fn pod_file_rejection(code: String, message: String) -> ApiError {
    let status = if code == "invalid_path" {
        StatusCode::BAD_REQUEST
    } else {
        StatusCode::BAD_GATEWAY
    };
    ApiError::new(status, code, message)
}

fn content_type(path: &str) -> &'static str {
    let extension = path
        .rsplit_once('.')
        .map(|(_, extension)| extension)
        .unwrap_or_default();
    if extension.eq_ignore_ascii_case("md") || extension.eq_ignore_ascii_case("markdown") {
        "text/markdown; charset=utf-8"
    } else if extension.eq_ignore_ascii_case("svg") {
        "image/svg+xml"
    } else if extension.eq_ignore_ascii_case("png") {
        "image/png"
    } else if extension.eq_ignore_ascii_case("jpg") || extension.eq_ignore_ascii_case("jpeg") {
        "image/jpeg"
    } else if extension.eq_ignore_ascii_case("gif") {
        "image/gif"
    } else if extension.eq_ignore_ascii_case("webp") {
        "image/webp"
    } else if extension.eq_ignore_ascii_case("avif") {
        "image/avif"
    } else if extension.eq_ignore_ascii_case("bmp") {
        "image/bmp"
    } else if extension.eq_ignore_ascii_case("ico") {
        "image/x-icon"
    } else if extension.eq_ignore_ascii_case("pdf") {
        "application/pdf"
    } else {
        "application/octet-stream"
    }
}

async fn control_upgrade(
    headers: HeaderMap,
    ws: WebSocketUpgrade,
    State(state): State<WebState>,
    Extension(session): Extension<AuthenticatedSession>,
    bridge: Option<Extension<TrustedFrontendBridge>>,
) -> Result<Response, ApiError> {
    if bridge.is_some() {
        exact_request_origin(&headers)?;
    } else {
        validate_browser_origin(&state, &headers)?;
    }
    let state = state.require_ready()?;
    Ok(ws
        .max_message_size(DEFAULT_MAX_FRAME_LEN)
        .max_frame_size(DEFAULT_MAX_FRAME_LEN)
        .on_upgrade(move |socket| control_session(socket, state, session)))
}

async fn control_session(socket: WebSocket, state: ReadyWebState, session: AuthenticatedSession) {
    let client_id = protocol::ClientId::generate();
    let _connection = state.host.auth().connect_browser(session.id.clone());
    let auth = state.host.auth().clone();
    let monitored_session_id = session.id.clone();
    let shutdown = async move {
        loop {
            tokio::time::sleep(Duration::from_secs(15)).await;
            match auth.keep_browser_alive(&monitored_session_id).await {
                Ok(true) => {}
                Ok(false) => return,
                Err(error) => {
                    warn!(%error, "failed to verify active browser session");
                    return;
                }
            }
        }
    };
    if let Err(error) = state
        .control
        .serve_browser_until_shutdown(WebSocketTransport { socket }, client_id, session, shutdown)
        .await
    {
        debug!(%error, "web control-plane connection closed");
    }
}

/// Carries complete control-plane messages over one web socket.
struct WebSocketTransport {
    socket: WebSocket,
}

impl control_plane::Transport for WebSocketTransport {
    async fn receive(&mut self) -> control_plane::Result<Option<protocol::Message>> {
        loop {
            let Some(message) = self.socket.recv().await else {
                return Ok(None);
            };
            let message = message.map_err(|error| {
                control_plane::Error::Transport
                    .report()
                    .message(error.to_string())
            })?;
            match message {
                WebSocketMessage::Text(text) => {
                    if text.len() > DEFAULT_MAX_FRAME_LEN {
                        return Err(control_plane::Error::FrameTooLarge {
                            len: text.len(),
                            max: DEFAULT_MAX_FRAME_LEN,
                        }
                        .report());
                    }
                    return serde_json::from_str(text.as_ref())
                        .map(Some)
                        .map_err(|error| {
                            control_plane::Error::InvalidMessage
                                .report()
                                .message(error.to_string())
                        });
                }
                WebSocketMessage::Binary(data) => {
                    if data.len() > DEFAULT_MAX_FRAME_LEN {
                        return Err(control_plane::Error::FrameTooLarge {
                            len: data.len(),
                            max: DEFAULT_MAX_FRAME_LEN,
                        }
                        .report());
                    }
                    return serde_json::from_slice(&data).map(Some).map_err(|error| {
                        control_plane::Error::InvalidMessage
                            .report()
                            .message(error.to_string())
                    });
                }
                WebSocketMessage::Ping(data) => {
                    self.socket
                        .send(WebSocketMessage::Pong(data))
                        .await
                        .map_err(|error| {
                            control_plane::Error::Transport
                                .report()
                                .message(error.to_string())
                        })?;
                }
                WebSocketMessage::Pong(_) => {}
                WebSocketMessage::Close(_) => return Ok(None),
            }
        }
    }

    async fn send(&mut self, message: protocol::Message) -> control_plane::Result<()> {
        let json = serde_json::to_string(&message).map_err(|error| {
            control_plane::Error::InvalidMessage
                .report()
                .message(error.to_string())
        })?;
        if json.len() > DEFAULT_MAX_FRAME_LEN {
            return Err(control_plane::Error::FrameTooLarge {
                len: json.len(),
                max: DEFAULT_MAX_FRAME_LEN,
            }
            .report());
        }
        self.socket
            .send(WebSocketMessage::Text(json.into()))
            .await
            .map_err(|error| {
                control_plane::Error::Transport
                    .report()
                    .message(error.to_string())
            })
    }
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    body: ApiErrorBody,
}

#[derive(Debug, Serialize)]
struct ApiErrorBody {
    message: String,
}

impl ApiError {
    fn bad_request(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, code, message)
    }

    fn forbidden(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, code, message)
    }

    fn unauthorized(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, code, message)
    }

    fn new(status: StatusCode, _code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            status,
            body: ApiErrorBody {
                message: message.into(),
            },
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(self.body)).into_response()
    }
}

#[cfg(test)]
mod cache_header_tests {
    use super::*;

    /// Prevents an SPA document from retaining stale asset references.
    #[test]
    fn spa_documents_are_not_cached() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/html; charset=utf-8"),
        );

        assert_eq!(
            ui_cache_control("/", StatusCode::OK, &headers),
            HeaderValue::from_static("no-store")
        );
        assert_eq!(
            ui_cache_control("/workspaces/demo", StatusCode::OK, &headers),
            HeaderValue::from_static("no-store")
        );
        assert_eq!(
            ui_cache_control("/assets/missing.js", StatusCode::OK, &headers),
            HeaderValue::from_static("no-store")
        );
    }

    /// Retains long-lived caching for content-addressed frontend assets.
    #[test]
    fn hashed_assets_are_cached_immutably() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/javascript"),
        );

        assert_eq!(
            ui_cache_control("/assets/index-CM0V8fSl.js", StatusCode::OK, &headers),
            HeaderValue::from_static("public, max-age=31536000, immutable")
        );
        assert_eq!(
            ui_cache_control("/assets/index-CM0V8fSl.js", StatusCode::NOT_FOUND, &headers),
            HeaderValue::from_static("no-store")
        );
        assert_eq!(
            ui_cache_control("/assets/index.js", StatusCode::OK, &headers),
            HeaderValue::from_static("no-store")
        );
        assert_eq!(
            ui_cache_control(
                "/assets/JetBrainsMonoNerdFontMono-SemiBold-BH6kv-6-.woff2",
                StatusCode::OK,
                &headers,
            ),
            HeaderValue::from_static("public, max-age=31536000, immutable")
        );
    }
}
