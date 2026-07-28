//! Browser-session cookies, origin checks, and HTTP-route authorization.
//!
//! This module is the HTTP authentication boundary used by
//! [`super::WebServer`]. It exchanges local pairing capabilities, enforces
//! browser sessions, and installs route-scoped credentials without exposing
//! tokens to applications.

use std::net::SocketAddr;
use std::time::Duration;

use axum::Json;
use axum::extract::Extension;
use axum::extract::Request;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::HeaderValue;
use axum::http::Method;
use axum::http::StatusCode;
use axum::http::Uri;
use axum::http::header;
use axum::middleware::Next;
use axum::response::Html;
use axum::response::IntoResponse;
use axum::response::Response;
use cookie::Cookie;
use cookie::SameSite;
use reportify::Report;
use tascarrel_api::types::auth as api;
use tascarrel_protocol::DEFAULT_MAX_FRAME_LEN;
use tracing::debug;

use super::ApiError;
use super::WebState;
use crate::services::auth::AuthServiceError;
use crate::services::auth::AuthenticatedSession;
use crate::services::auth::HTTP_ROUTE_COOKIE_NAME;
use crate::services::network::ResolvedHttpRoute;

/// Marks a request authenticated through a trusted frontend route.
#[derive(Clone, Copy, Debug)]
pub(crate) struct TrustedFrontendBridge;

/// Host-only cookie carrying one durable browser credential.
pub(crate) const BROWSER_SESSION_COOKIE: &str = "tascarrel_session";
/// Reserved route endpoint that exchanges one fragment-delivered ticket.
pub(crate) const ROUTE_AUTHORIZATION_PATH: &str = "/.tascarrel/authorize";
const ROUTE_BRIDGE_PREFIX: &str = "/.tascarrel";

/// Requires a valid browser session before entering a protected API route.
pub(crate) async fn require_browser_session(
    State(state): State<WebState>,
    mut request: Request,
    next: Next,
) -> Response {
    if request
        .headers()
        .get("sec-fetch-site")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|site| site != "same-origin")
        && exact_request_origin(request.headers()).is_err()
    {
        return ApiError::forbidden(
            "cross-origin-request",
            "Tascarrel API requests must be same-origin",
        )
        .into_response();
    }
    if request.extensions().get::<AuthenticatedSession>().is_some() {
        return next.run(request).await;
    }
    let Some(token) = cookie_value(request.headers(), BROWSER_SESSION_COOKIE) else {
        return ApiError::unauthorized(
            "authentication-required",
            "a paired Tascarrel browser session is required",
        )
        .into_response();
    };
    match state.auth.authenticate_browser(&token).await {
        Ok(session) => {
            request.extensions_mut().insert(session);
            next.run(request).await
        }
        Err(error) if error.error() == &AuthServiceError::InvalidSession => {
            debug!(%error, "browser session authentication failed");
            ApiError::unauthorized(
                "invalid-session",
                "the Tascarrel browser session is invalid or expired",
            )
            .into_response()
        }
        Err(error) => authentication_service_error(&error).into_response(),
    }
}

/// Exchanges a local-admin pairing key for a browser cookie.
///
/// # Errors
///
/// Returns an HTTP API error when the request origin or pairing key is invalid
/// or the session cookie cannot be encoded.
pub(crate) async fn pair_browser(
    State(state): State<WebState>,
    headers: HeaderMap,
    Json(input): Json<api::PairBrowserRequest>,
) -> Result<Response, ApiError> {
    let origin = validate_browser_origin(&state, &headers)?;
    let session = state
        .auth
        .pair_browser(&input.pairing_key, input.label.as_deref(), &origin)
        .await
        .map_err(|error| pairing_api_error(&error))?;
    let secure = origin.starts_with("https://");
    let mut response = StatusCode::NO_CONTENT.into_response();
    set_cookie(
        response.headers_mut(),
        BROWSER_SESSION_COOKIE,
        &session.cookie_token,
        secure,
        cookie_max_age(state.auth.absolute_session_lifetime()),
    )?;
    Ok(response)
}

/// Confirms that the authentication middleware accepted this request.
pub(crate) async fn authenticated_session() -> StatusCode {
    StatusCode::NO_CONTENT
}

/// Returns the same-origin API root visible to this frontend.
pub(crate) async fn frontend_context(
    bridge: Option<Extension<TrustedFrontendBridge>>,
) -> Json<api::BrowserAuthContext> {
    Json(api::BrowserAuthContext {
        api_root: if bridge.is_some() {
            "/.tascarrel/api/v1"
        } else {
            "/api/v1"
        }
        .into(),
    })
}

/// Revokes the current browser and clears its session cookie.
///
/// # Errors
///
/// Returns an HTTP API error when the request origin is invalid, durable
/// authentication state is unavailable, or the cookie cannot be cleared.
pub(crate) async fn logout_browser(
    State(state): State<WebState>,
    Extension(session): Extension<AuthenticatedSession>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let origin = validate_browser_origin(&state, &headers)?;
    state
        .auth
        .revoke_session(&session.id)
        .await
        .map_err(|error| authentication_service_error(&error))?;
    let mut response = StatusCode::NO_CONTENT.into_response();
    clear_cookie(
        response.headers_mut(),
        BROWSER_SESSION_COOKIE,
        origin.starts_with("https://"),
    )?;
    Ok(response)
}

/// Returns the configured local or public origin matching this request.
///
/// # Errors
///
/// Returns an HTTP API error when the request does not prove an allowed exact
/// origin.
pub(crate) fn validate_browser_origin(
    state: &WebState,
    headers: &HeaderMap,
) -> Result<String, ApiError> {
    let host = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| ApiError::forbidden("invalid-origin", "missing HTTP host"))?;
    let origin = headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<Uri>().ok())
        .filter(|origin| {
            matches!(origin.scheme_str(), Some("http" | "https"))
                && origin.path() == "/"
                && origin.query().is_none()
        })
        .ok_or_else(|| ApiError::forbidden("invalid-origin", "missing or invalid HTTP origin"))?;
    let authority = origin
        .authority()
        .ok_or_else(|| ApiError::forbidden("invalid-origin", "origin has no authority"))?;
    if !authority.as_str().eq_ignore_ascii_case(host) {
        return Err(ApiError::forbidden(
            "cross-origin-request",
            "request origin does not match its HTTP host",
        ));
    }
    let scheme = origin
        .scheme_str()
        .ok_or_else(|| ApiError::forbidden("invalid-origin", "origin has no scheme"))?;
    let canonical = format!("{scheme}://{authority}");
    if state
        .public_origin
        .as_ref()
        .is_some_and(|public| public.eq_ignore_ascii_case(&canonical))
        || is_local_browser_origin(&origin, state.web_authority)
    {
        return Ok(canonical);
    }
    Err(ApiError::forbidden(
        "untrusted-origin",
        "browser origin is not configured for this Tascarrel host",
    ))
}

/// Validates that one browser request proves its exact HTTP authority.
///
/// # Errors
///
/// Returns an HTTP API error when the request has no valid same-origin proof.
pub(crate) fn exact_request_origin(headers: &HeaderMap) -> Result<Uri, ApiError> {
    let host = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| ApiError::forbidden("invalid-origin", "missing HTTP host"))?;
    let origin = headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<Uri>().ok())
        .filter(|origin| matches!(origin.scheme_str(), Some("http" | "https")))
        .ok_or_else(|| ApiError::forbidden("invalid-origin", "missing or invalid HTTP origin"))?;
    if !origin
        .authority()
        .is_some_and(|authority| authority.as_str().eq_ignore_ascii_case(host))
    {
        return Err(ApiError::forbidden(
            "cross-origin-request",
            "request origin does not match its HTTP host",
        ));
    }
    Ok(origin)
}

/// Returns one named cookie without exposing parse failures as credentials.
pub(crate) fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    for value in headers.get_all(header::COOKIE) {
        let Ok(value) = value.to_str() else {
            debug!("ignoring a non-text browser Cookie header");
            continue;
        };
        for cookie in Cookie::split_parse(value) {
            match cookie {
                Ok(cookie) if cookie.name() == name => {
                    return Some(cookie.value().to_owned());
                }
                Ok(_) => {}
                Err(error) => debug!(%error, "ignoring an invalid browser cookie"),
            }
        }
    }
    None
}

/// Serves or exchanges a route authorization ticket.
pub(crate) async fn route_authorization(
    state: &WebState,
    request: Request,
    route: &ResolvedHttpRoute,
) -> Response {
    if request.method() == Method::GET {
        return route_authorization_page();
    }
    if request.method() != Method::POST {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    let origin = match exact_request_origin(request.headers()) {
        Ok(origin) => origin,
        Err(error) => return error.into_response(),
    };
    if !origin
        .authority()
        .is_some_and(|authority| authority.host().eq_ignore_ascii_case(route.hostname()))
    {
        return ApiError::forbidden(
            "cross-origin-request",
            "route exchange origin does not match the route hostname",
        )
        .into_response();
    }
    let bytes = match axum::body::to_bytes(request.into_body(), DEFAULT_MAX_FRAME_LEN).await {
        Ok(bytes) => bytes,
        Err(error) => {
            return ApiError::bad_request("invalid-ticket", error.to_string()).into_response();
        }
    };
    let input = match serde_json::from_slice::<api::ExchangeHttpRouteTicketRequest>(&bytes) {
        Ok(input) => input,
        Err(error) => {
            return ApiError::bad_request("invalid-ticket", error.to_string()).into_response();
        }
    };
    let grant = match state
        .auth
        .exchange_route_ticket(&input.ticket, route.hostname())
        .await
    {
        Ok(grant) if &grant.route_id == route.id() => grant,
        Ok(_) => {
            return ApiError::unauthorized(
                "invalid-route-ticket",
                "the route ticket is scoped to another route",
            )
            .into_response();
        }
        Err(error)
            if matches!(
                error.error(),
                AuthServiceError::InvalidRouteTicket | AuthServiceError::InvalidSession
            ) =>
        {
            debug!(%error, "HTTP route ticket exchange failed");
            return ApiError::unauthorized(
                "invalid-route-ticket",
                "the route ticket is invalid or expired",
            )
            .into_response();
        }
        Err(error) => return authentication_service_error(&error).into_response(),
    };
    let mut response = Json(api::ExchangeHttpRouteTicketOutput {
        return_to: grant.return_to.into(),
    })
    .into_response();
    let secure = origin.scheme_str() == Some("https");
    if let Err(error) = set_cookie(
        response.headers_mut(),
        HTTP_ROUTE_COOKIE_NAME,
        &grant.cookie_token,
        secure,
        cookie_max_age(state.auth.route_grant_lifetime()),
    ) {
        return error.into_response();
    }
    response
}

/// Returns an uncached response explaining that a route ticket is required.
pub(crate) fn route_authentication_required() -> Response {
    let mut response = (
        StatusCode::UNAUTHORIZED,
        "This Tascarrel HTTP route requires an access ticket.\n",
    )
        .into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// Rewrites a reserved trusted-route path to its host API target.
pub(crate) fn trusted_bridge_uri(uri: &Uri) -> Option<Uri> {
    let path_and_query = uri.path_and_query()?.as_str();
    let rewritten = if path_and_query == "/.tascarrel/context" {
        "/api/v1/auth/context"
    } else {
        path_and_query.strip_prefix(ROUTE_BRIDGE_PREFIX)?
    };
    if !rewritten.starts_with("/api/") {
        return None;
    }
    match rewritten.parse() {
        Ok(uri) => Some(uri),
        Err(error) => {
            debug!(%error, "could not rewrite a trusted frontend API URI");
            None
        }
    }
}

/// Returns whether a path belongs to hostd's reserved route bridge namespace.
pub(crate) fn is_route_bridge_path(path: &str) -> bool {
    path == ROUTE_BRIDGE_PREFIX || path.starts_with("/.tascarrel/")
}

fn is_local_browser_origin(origin: &Uri, web_authority: SocketAddr) -> bool {
    if origin.scheme_str() != Some("http") {
        return false;
    }
    let Some(authority) = origin.authority() else {
        return false;
    };
    if authority.port_u16() != Some(web_authority.port()) {
        return false;
    }
    let host = authority.host();
    host.eq_ignore_ascii_case("localhost")
        || host.eq_ignore_ascii_case("tascarrel.localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

fn set_cookie(
    headers: &mut HeaderMap,
    name: &'static str,
    value: &str,
    secure: bool,
    max_age: cookie::time::Duration,
) -> Result<(), ApiError> {
    let cookie = Cookie::build((name, value.to_owned()))
        .path("/")
        .http_only(true)
        .secure(secure)
        .same_site(SameSite::Strict)
        .max_age(max_age)
        .build();
    headers.append(
        header::SET_COOKIE,
        HeaderValue::from_str(&cookie.to_string()).map_err(|_| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "invalid-cookie",
                "could not encode the authentication cookie",
            )
        })?,
    );
    Ok(())
}

fn clear_cookie(headers: &mut HeaderMap, name: &'static str, secure: bool) -> Result<(), ApiError> {
    let cookie = Cookie::build((name, ""))
        .path("/")
        .http_only(true)
        .secure(secure)
        .same_site(SameSite::Strict)
        .max_age(cookie::time::Duration::ZERO)
        .build();
    headers.append(
        header::SET_COOKIE,
        HeaderValue::from_str(&cookie.to_string()).map_err(|_| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "invalid-cookie",
                "could not clear the authentication cookie",
            )
        })?,
    );
    Ok(())
}

fn cookie_max_age(duration: Duration) -> cookie::time::Duration {
    let seconds = i64::try_from(duration.as_secs())
        .expect("authentication configuration validates cookie lifetime seconds");
    cookie::time::Duration::seconds(seconds)
}

/// Converts an authentication infrastructure failure to a safe HTTP error.
pub(crate) fn authentication_service_error(error: &Report<AuthServiceError>) -> ApiError {
    debug!(%error, "authentication service request failed");
    ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "authentication-unavailable",
        "the authentication service is unavailable",
    )
}

fn pairing_api_error(error: &Report<AuthServiceError>) -> ApiError {
    debug!(%error, "browser pairing failed");
    match error.error() {
        AuthServiceError::InvalidPairingKey => ApiError::unauthorized(
            "invalid-pairing-key",
            "the pairing key is invalid or expired",
        ),
        AuthServiceError::InvalidRequest => {
            ApiError::bad_request("invalid-pairing-request", "the pairing request is invalid")
        }
        AuthServiceError::Capacity => ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "pairing-capacity-exhausted",
            "the host cannot create another browser session right now",
        ),
        AuthServiceError::Unavailable
        | AuthServiceError::InvalidSession
        | AuthServiceError::InvalidRouteTicket
        | AuthServiceError::InvalidRouteGrant => authentication_service_error(error),
    }
}

fn route_authorization_page() -> Response {
    let document = r#"<!doctype html>
<html lang="en">
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Opening Tascarrel route…</title>
<p id="status">Authorizing this Tascarrel route…</p>
<script>
const status = document.getElementById("status");
const ticket = new URLSearchParams(location.hash.slice(1)).get("ticket");
history.replaceState(null, "", location.pathname);
if (!ticket) {
  status.textContent = "This route access link is incomplete.";
} else {
  fetch(location.pathname, {
    method: "POST",
    credentials: "same-origin",
    headers: {"content-type": "application/json"},
    body: JSON.stringify({ticket})
  }).then(async response => {
    if (!response.ok) throw new Error("Route authorization failed.");
    const result = await response.json();
    location.replace(result.returnTo);
  }).catch(error => { status.textContent = error.message; });
}
</script>
"#;
    let mut response = Html(document).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response.headers_mut().insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'none'; connect-src 'self'; script-src 'unsafe-inline'; base-uri 'none'; form-action 'none'",
        ),
    );
    response.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::RwLock;

    use axum::body::Body;
    use tascarrel_api::types::auth as api;
    use tokio::sync::mpsc;
    use tower::ServiceExt as _;

    use super::*;
    use crate::services::auth::AuthService;
    use crate::services::auth::AuthServiceConfig;
    use crate::startup::StartupReporter;
    use crate::web::router;

    async fn bootstrap_state(directory: &std::path::Path) -> WebState {
        let auth = AuthService::open(AuthServiceConfig::new(directory))
            .await
            .unwrap();
        let (retry, _) = mpsc::channel(1);
        WebState {
            auth,
            public_origin: None,
            ready: Arc::new(RwLock::new(None)),
            retry,
            status: StartupReporter::new(),
            web_authority: "127.0.0.1:8272".parse().unwrap(),
        }
    }

    /// Accepts the browser origin only when both authorities match the bound
    /// listener.
    #[tokio::test]
    async fn browser_origin_matches_bound_authority() {
        let directory = tempfile::tempdir().unwrap();
        let state = bootstrap_state(directory.path()).await;
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, "127.0.0.1:8272".parse().unwrap());
        headers.insert(header::ORIGIN, "http://127.0.0.1:8272".parse().unwrap());

        assert!(validate_browser_origin(&state, &headers).is_ok());
    }

    /// Accepts the canonical frontend hostname while hostd remains bound to
    /// its loopback socket address.
    #[tokio::test]
    async fn browser_origin_accepts_canonical_frontend_authority() {
        let directory = tempfile::tempdir().unwrap();
        let state = bootstrap_state(directory.path()).await;
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, "tascarrel.localhost:8272".parse().unwrap());
        headers.insert(
            header::ORIGIN,
            "http://tascarrel.localhost:8272".parse().unwrap(),
        );

        assert!(validate_browser_origin(&state, &headers).is_ok());
    }

    /// Rejects a canonical hostname carrying a port other than hostd's bound
    /// frontend port.
    #[tokio::test]
    async fn browser_origin_rejects_wrong_canonical_port() {
        let directory = tempfile::tempdir().unwrap();
        let state = bootstrap_state(directory.path()).await;
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, "tascarrel.localhost:8273".parse().unwrap());
        headers.insert(
            header::ORIGIN,
            "http://tascarrel.localhost:8273".parse().unwrap(),
        );

        assert!(validate_browser_origin(&state, &headers).is_err());
    }

    /// Rejects non-browser clients that omit the required origin proof.
    #[tokio::test]
    async fn browser_origin_requires_origin() {
        let directory = tempfile::tempdir().unwrap();
        let state = bootstrap_state(directory.path()).await;
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, "127.0.0.1:8272".parse().unwrap());

        assert!(validate_browser_origin(&state, &headers).is_err());
    }

    /// Rejects a rebound hostname even when its Origin and Host agree.
    #[tokio::test]
    async fn browser_origin_rejects_rebound_authority() {
        let directory = tempfile::tempdir().unwrap();
        let state = bootstrap_state(directory.path()).await;
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, "attacker.example:8272".parse().unwrap());
        headers.insert(
            header::ORIGIN,
            "http://attacker.example:8272".parse().unwrap(),
        );

        assert!(validate_browser_origin(&state, &headers).is_err());
    }

    /// Verifies browser pairing installs an HTTP-only credential required by
    /// protected API routes and rejects sibling-origin use.
    #[tokio::test]
    async fn pairing_installs_a_cookie_required_by_browser_api_routes() {
        let directory = tempfile::tempdir().unwrap();
        let state = bootstrap_state(directory.path()).await;
        let pairing = state.auth.create_pairing_key(None).unwrap();
        let request = Request::builder()
            .method(Method::POST)
            .uri("/api/v1/auth/pair")
            .header(header::HOST, "127.0.0.1:8272")
            .header(header::ORIGIN, "http://127.0.0.1:8272")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::to_vec(&api::PairBrowserRequest {
                    pairing_key: pairing.pairing_key,
                    label: None,
                })
                .unwrap(),
            ))
            .unwrap();
        let response = router(state.clone()).oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        let cookie = response
            .headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        assert!(cookie.starts_with("tascarrel_session="));
        assert!(
            response
                .headers()
                .get(header::SET_COOKIE)
                .unwrap()
                .to_str()
                .unwrap()
                .contains("HttpOnly")
        );

        let authenticated = Request::builder()
            .uri("/api/v1/auth/session")
            .header(header::COOKIE, cookie.clone())
            .header("sec-fetch-site", "same-origin")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            router(state.clone())
                .oneshot(authenticated)
                .await
                .unwrap()
                .status(),
            StatusCode::NO_CONTENT
        );

        let cross_origin = Request::builder()
            .uri("/api/v1/auth/session")
            .header(header::COOKIE, cookie)
            .header("sec-fetch-site", "same-site")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            router(state).oneshot(cross_origin).await.unwrap().status(),
            StatusCode::FORBIDDEN
        );
    }
}
