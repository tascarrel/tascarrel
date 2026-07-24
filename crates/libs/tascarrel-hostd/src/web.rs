//! Loopback-only static HTTP plus the Tascarrel control `WebSocket`.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use axum::Json;
use axum::Router;
use axum::body::Body;
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
use axum::http::uri::Authority;
use axum::middleware;
use axum::middleware::Next;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing::get;
use axum::routing::post;
use futures_util::StreamExt;
use reportify::ErrorExt as _;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use tascarrel_api::types::protocol;
use tascarrel_protocol::ChatAttachmentReadRequest;
use tascarrel_protocol::ChatAttachmentReadResponse;
use tascarrel_protocol::ChatAttachmentUploadRequest;
use tascarrel_protocol::ChatAttachmentUploadResponse;
use tascarrel_protocol::DEFAULT_MAX_FRAME_LEN;
use tascarrel_protocol::FrameReader;
use tascarrel_protocol::Framed;
use tascarrel_protocol::MAX_CHAT_ATTACHMENT_BYTES;
use tascarrel_protocol::MUX_CHAT_ATTACHMENT_READ_ENDPOINT;
use tascarrel_protocol::MUX_CHAT_ATTACHMENT_UPLOAD_ENDPOINT;
use tascarrel_protocol::MUX_WORKSPACE_FILE_READ_ENDPOINT;
use tascarrel_protocol::WorkspaceFileReadRequest;
use tascarrel_protocol::WorkspaceFileReadResponse;
use tascarrel_protocol::WorkspaceName;
use tascarrel_protocol::control_plane;
use tokio::io::AsyncWriteExt as _;
use tokio::net::TcpListener;
use tokio_util::io::ReaderStream;
use tower_http::cors::AllowOrigin;
use tower_http::cors::CorsLayer;
use tower_http::services::ServeDir;
use tower_http::services::ServeFile;
use tracing::debug;
use tracing::info;

use crate::HostState;
use crate::control_plane::HostControlService;

const FRONTEND_CORS_MAX_AGE: Duration = Duration::from_secs(600);
const UI_DOCUMENT_CACHE_CONTROL: HeaderValue = HeaderValue::from_static("no-store");
const UI_ASSET_CACHE_CONTROL: HeaderValue =
    HeaderValue::from_static("public, max-age=31536000, immutable");
const CHAT_ATTACHMENT_UPLOAD_PROOF: &str = "tascarrel-chat-attachment";

#[derive(Clone, Debug)]
pub struct WebServerConfig {
    pub address: SocketAddr,
    pub ui_root: Option<PathBuf>,
}

#[derive(Clone)]
struct WebState {
    host: HostState,
    control: HostControlService,
    web_authority: SocketAddr,
}

impl WebState {
    fn workspace_service(&self) -> &crate::WorkspaceService {
        self.host.workspaces()
    }

    fn network_service(&self) -> &crate::NetworkService {
        self.host.network()
    }
}

/// Serves the extracted frontend and host-owned streaming transports.
///
/// # Errors
///
/// Returns an error when the TCP listener cannot be bound or the Axum server
/// fails.
pub(crate) async fn serve(
    config: WebServerConfig,
    state: HostState,
    control: HostControlService,
) -> anyhow::Result<()> {
    let listener = TcpListener::bind(config.address).await?;
    let web_authority = listener.local_addr()?;
    info!(address = %web_authority, "Tascarrel web interface ready");
    axum::serve(
        listener,
        router(
            WebState {
                host: state,
                control,
                web_authority,
            },
            config.ui_root,
        ),
    )
    .await?;
    Ok(())
}

fn router(state: WebState, ui_root: Option<PathBuf>) -> Router {
    let network_state = state.clone();
    let cors = frontend_cors(&state);
    let router = Router::new()
        .route("/api/health", get(health))
        .route("/api/v1/control", get(control_upgrade))
        .route(
            "/api/v1/chat/upload-attachment",
            post(upload_chat_attachment),
        )
        .route("/api/v1/chat/attachment", get(read_chat_attachment))
        .route("/api/v1/files/raw", get(raw_file))
        .with_state(state)
        .layer(cors);

    let router = if let Some(ui_root) = ui_root {
        let index = ui_root.join("index.html");
        router
            .fallback_service(ServeDir::new(ui_root).not_found_service(ServeFile::new(index)))
            .layer(middleware::from_fn(ui_cache_headers))
    } else {
        router
    };

    router.layer(middleware::from_fn_with_state(
        network_state,
        forward_network_request,
    ))
}

/// Builds the browser API policy for canonical and explicitly trusted origins.
fn frontend_cors(state: &WebState) -> CorsLayer {
    let network = state.network_service().clone();
    let web_authority = state.web_authority;
    CorsLayer::new()
        .allow_origin(AllowOrigin::predicate(move |origin, _request| {
            is_allowed_frontend_origin(origin, web_authority, &network)
        }))
        .allow_methods([Method::GET, Method::POST])
        .allow_headers([
            header::CONTENT_TYPE,
            header::HeaderName::from_static("x-tascarrel-request"),
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
    request: Request,
    next: Next,
) -> Response {
    let route = match state
        .network_service()
        .resolve_http_route(request.headers())
    {
        Ok(Some(route)) => route,
        Ok(None) => return next.run(request).await,
        Err(error) => return network_proxy_error(&error),
    };
    match state
        .network_service()
        .forward_http(request, route, state.workspace_service())
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

async fn health() -> Json<Value> {
    Json(serde_json::json!({ "status": "ok" }))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct UploadChatAttachmentQuery {
    workspace: String,
    name: String,
}

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
    path: String,
    #[serde(default)]
    download: bool,
}

#[tracing::instrument(
    level = "debug",
    skip_all,
    fields(workspace = %input.workspace, pod_id = %input.pod_id, path = %input.path)
)]
async fn raw_file(
    State(state): State<WebState>,
    Query(input): Query<RawFileQuery>,
) -> Result<Response, ApiError> {
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
        .open(MUX_WORKSPACE_FILE_READ_ENDPOINT)
        .await
        .map_err(|error| {
            ApiError::new(
                StatusCode::BAD_GATEWAY,
                "file-read-unavailable",
                format!("failed to open workspace file read: {error}"),
            )
        })?;
    let mut framed = Framed::new(channel);
    framed
        .write(&WorkspaceFileReadRequest {
            pod_id,
            path: tascarrel_api::types::files::FilePath::new(input.path.clone()),
        })
        .await
        .map_err(|error| {
            ApiError::new(
                StatusCode::BAD_GATEWAY,
                "file-read-failed",
                format!("failed to send workspace file request: {error}"),
            )
        })?;
    framed.get_mut().shutdown().await.map_err(|error| {
        ApiError::new(
            StatusCode::BAD_GATEWAY,
            "file-read-failed",
            format!("failed to finish workspace file request: {error}"),
        )
    })?;
    let result = framed
        .read::<WorkspaceFileReadResponse>()
        .await
        .map_err(|error| {
            ApiError::new(
                StatusCode::BAD_GATEWAY,
                "file-read-failed",
                format!("failed to read workspace file response: {error}"),
            )
        })?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::BAD_GATEWAY,
                "file-read-failed",
                "workspace file read closed without a response",
            )
        })?;
    let size = match result {
        WorkspaceFileReadResponse::Found { size } => size,
        WorkspaceFileReadResponse::Rejected { code, message } => {
            return Err(workspace_file_rejection(code, message));
        }
    };
    let mut response = Response::new(Body::from_stream(ReaderStream::new(framed.into_inner())));
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(content_type(&input.path)),
    );
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(size));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    if input.path.to_ascii_lowercase().ends_with(".svg") {
        headers.insert(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static("sandbox; default-src 'none'; style-src 'unsafe-inline'"),
        );
    }
    if input.download {
        headers.insert(
            header::CONTENT_DISPOSITION,
            HeaderValue::from_static("attachment"),
        );
    }
    Ok(response)
}

fn workspace_file_rejection(code: String, message: String) -> ApiError {
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
) -> Result<Response, ApiError> {
    validate_websocket_origin(&headers, state.web_authority, state.network_service())?;
    Ok(ws
        .max_message_size(DEFAULT_MAX_FRAME_LEN)
        .max_frame_size(DEFAULT_MAX_FRAME_LEN)
        .on_upgrade(move |socket| control_session(socket, state)))
}

async fn control_session(socket: WebSocket, state: WebState) {
    let client_id = protocol::ClientId::generate();
    if let Err(error) = state
        .control
        .serve(WebSocketTransport { socket }, client_id)
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

fn validate_websocket_origin(
    headers: &HeaderMap,
    web_authority: SocketAddr,
    network: &crate::NetworkService,
) -> Result<(), ApiError> {
    let origin = headers
        .get(header::ORIGIN)
        .ok_or_else(|| ApiError::forbidden("missing-origin", "missing WebSocket origin"))?;
    let origin = origin
        .to_str()
        .map_err(|_| ApiError::forbidden("invalid-origin", "invalid WebSocket origin"))?;
    let origin = origin
        .parse::<Uri>()
        .map_err(|_| ApiError::forbidden("invalid-origin", "invalid WebSocket origin"))?;
    if !matches!(origin.scheme_str(), Some("http" | "https")) {
        return Err(ApiError::forbidden(
            "invalid-origin",
            "invalid WebSocket origin",
        ));
    }
    let origin_authority = origin
        .authority()
        .ok_or_else(|| ApiError::forbidden("invalid-origin", "invalid WebSocket origin"))?;
    let request_authority: Authority = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| ApiError::forbidden("invalid-origin", "missing HTTP host"))?
        .parse()
        .map_err(|_| ApiError::forbidden("invalid-origin", "invalid HTTP host"))?;
    let same_origin = origin_authority
        .host()
        .eq_ignore_ascii_case(request_authority.host())
        && origin_authority.port_u16() == request_authority.port_u16();
    let trusted_frontend_origin =
        network.is_trusted_tascarrel_frontend_authority(origin_authority, web_authority);
    if !network.is_frontend_authority(&request_authority, web_authority)
        || (!same_origin && !trusted_frontend_origin)
    {
        return Err(ApiError::forbidden(
            "cross-origin-websocket",
            "WebSocket authority does not match the configured web interface",
        ));
    }
    Ok(())
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
mod websocket_origin_tests {
    use super::*;

    fn network() -> crate::NetworkService {
        crate::NetworkService::new(crate::NetworkServiceConfig::default()).unwrap()
    }

    /// Accepts the browser origin only when both authorities match the bound
    /// listener.
    #[test]
    fn browser_websocket_origin_matches_bound_authority() {
        let authority = "127.0.0.1:8272".parse().unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, "127.0.0.1:8272".parse().unwrap());
        headers.insert(header::ORIGIN, "http://127.0.0.1:8272".parse().unwrap());

        assert!(validate_websocket_origin(&headers, authority, &network()).is_ok());
    }

    /// Accepts the canonical frontend hostname while hostd remains bound to
    /// its loopback socket address.
    #[test]
    fn browser_websocket_origin_accepts_canonical_frontend_authority() {
        let authority = "127.0.0.1:8272".parse().unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, "tascarrel.localhost:8272".parse().unwrap());
        headers.insert(
            header::ORIGIN,
            "http://tascarrel.localhost:8272".parse().unwrap(),
        );

        assert!(validate_websocket_origin(&headers, authority, &network()).is_ok());
    }

    /// Rejects a canonical hostname carrying a port other than hostd's bound
    /// frontend port.
    #[test]
    fn browser_websocket_origin_rejects_wrong_canonical_port() {
        let authority = "127.0.0.1:8272".parse().unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, "tascarrel.localhost:8273".parse().unwrap());
        headers.insert(
            header::ORIGIN,
            "http://tascarrel.localhost:8273".parse().unwrap(),
        );

        assert!(validate_websocket_origin(&headers, authority, &network()).is_err());
    }

    /// Rejects non-browser clients that omit the required origin proof.
    #[test]
    fn browser_websocket_requires_origin() {
        let authority = "127.0.0.1:8272".parse().unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, "127.0.0.1:8272".parse().unwrap());

        assert!(validate_websocket_origin(&headers, authority, &network()).is_err());
    }

    /// Rejects a rebound hostname even when its Origin and Host agree.
    #[test]
    fn browser_websocket_rejects_rebound_authority() {
        let authority = "127.0.0.1:8272".parse().unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, "attacker.example:8272".parse().unwrap());
        headers.insert(
            header::ORIGIN,
            "http://attacker.example:8272".parse().unwrap(),
        );

        assert!(validate_websocket_origin(&headers, authority, &network()).is_err());
    }

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
