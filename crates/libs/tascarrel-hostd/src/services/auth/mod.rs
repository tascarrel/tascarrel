//! Host-owned browser pairing, durable sessions, and route grants.
//!
//! [`AuthService`] stores only keyed token digests in `SQLite`. Pairing and
//! route tickets are short-lived in-memory capabilities; durable browser and
//! route cookies contain independently generated opaque secrets.

mod storage;

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Read as _;
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::Hmac;
use hmac::Mac as _;
use jiff::Timestamp;
use rand::Rng as _;
use reportify::ErrorExt as _;
use reportify::Report;
use sha2::Sha256;
use tascarrel_api::types::auth as api;
use tascarrel_api::types::network::HttpRouteId;
use thiserror::Error;
use tokio::sync::watch;
use tracing::warn;

use self::storage::AuthStorage;
use self::storage::NewBrowserSession;
use self::storage::NewRouteGrant;
use self::storage::SessionActivity;
use crate::services::workspaces::create_private_directory;

const AUTH_SECRET_BYTES: usize = 32;
const OPAQUE_TOKEN_BYTES: usize = 32;
const AUTH_SECRET_FILE: &str = "key";
const AUTH_DATABASE_FILE: &str = "sessions.sqlite3";
const DEFAULT_PAIRING_LIFETIME: Duration = Duration::from_mins(10);
const DEFAULT_ROUTE_TICKET_LIFETIME: Duration = Duration::from_mins(1);
const DEFAULT_IDLE_SESSION_LIFETIME: Duration = Duration::from_hours(720);
const DEFAULT_ABSOLUTE_SESSION_LIFETIME: Duration = Duration::from_hours(4320);
const DEFAULT_SESSION_TOUCH_INTERVAL: Duration = Duration::from_hours(1);
const DEFAULT_ROUTE_GRANT_LIFETIME: Duration = Duration::from_hours(24);
const DEFAULT_MAX_PENDING_PAIRING_KEYS: usize = 64;
const DEFAULT_MAX_PENDING_ROUTE_TICKETS: usize = 1024;
const MAX_SESSION_LABEL_BYTES: usize = 80;
pub(crate) const HTTP_ROUTE_COOKIE_NAME: &str = "tascarrel_route";

type HmacSha256 = Hmac<Sha256>;

/// Paths and lifetimes used by the host authentication service.
#[derive(Clone, Debug)]
pub struct AuthServiceConfig {
    /// Private directory containing the authentication key and database.
    pub state_directory: PathBuf,
    /// Optional externally managed authentication key file.
    pub secret_file: Option<PathBuf>,
    /// Lifetime of one unconsumed pairing key.
    pub pairing_lifetime: Duration,
    /// Lifetime of one unconsumed route ticket.
    pub route_ticket_lifetime: Duration,
    /// Maximum inactivity accepted for a browser session.
    pub idle_session_lifetime: Duration,
    /// Maximum absolute browser-session lifetime.
    pub absolute_session_lifetime: Duration,
    /// Minimum interval between durable last-seen updates.
    pub session_touch_interval: Duration,
    /// Maximum lifetime of one route grant.
    pub route_grant_lifetime: Duration,
    /// Maximum number of unconsumed pairing capabilities retained in memory.
    pub max_pending_pairing_keys: usize,
    /// Maximum number of unconsumed route capabilities retained in memory.
    pub max_pending_route_tickets: usize,
}

impl AuthServiceConfig {
    /// Creates the standard authentication configuration below host state.
    #[must_use]
    pub fn new(state_directory: impl AsRef<Path>) -> Self {
        Self {
            state_directory: state_directory.as_ref().to_owned(),
            secret_file: None,
            pairing_lifetime: DEFAULT_PAIRING_LIFETIME,
            route_ticket_lifetime: DEFAULT_ROUTE_TICKET_LIFETIME,
            idle_session_lifetime: DEFAULT_IDLE_SESSION_LIFETIME,
            absolute_session_lifetime: DEFAULT_ABSOLUTE_SESSION_LIFETIME,
            session_touch_interval: DEFAULT_SESSION_TOUCH_INTERVAL,
            route_grant_lifetime: DEFAULT_ROUTE_GRANT_LIFETIME,
            max_pending_pairing_keys: DEFAULT_MAX_PENDING_PAIRING_KEYS,
            max_pending_route_tickets: DEFAULT_MAX_PENDING_ROUTE_TICKETS,
        }
    }
}

/// Host-owned authentication state shared by HTTP and control-plane clients.
#[derive(Clone)]
pub struct AuthService {
    inner: Arc<AuthServiceInner>,
}

impl std::fmt::Debug for AuthService {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AuthService")
            .field("secret", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}

struct AuthServiceInner {
    storage: AuthStorage,
    secret: [u8; AUTH_SECRET_BYTES],
    config: AuthServiceConfig,
    pending: Mutex<PendingCapabilities>,
    active_connections: Mutex<HashMap<api::BrowserSessionId, u32>>,
    changes: watch::Sender<u64>,
}

#[derive(Default)]
struct PendingCapabilities {
    pairing_keys: HashMap<[u8; 32], PendingPairingKey>,
    route_tickets: HashMap<[u8; 32], PendingRouteTicket>,
}

struct PendingPairingKey {
    expires_at: Timestamp,
    label: Option<String>,
}

/// Route target retained until one browser consumes its ticket.
#[derive(Clone, Debug)]
pub struct PendingRouteTicket {
    /// Browser session from which this route access was delegated.
    pub session_id: api::BrowserSessionId,
    /// Stable HTTP route receiving the grant.
    pub route_id: HttpRouteId,
    /// Exact route hostname on which the grant may be installed.
    pub hostname: String,
    /// Target path, query, and fragment restored after the exchange.
    pub return_to: String,
    expires_at: Timestamp,
}

/// Authenticated durable browser session.
#[derive(Clone, Debug)]
pub struct AuthenticatedSession {
    /// Stable identifier of the authenticated browser session.
    pub id: api::BrowserSessionId,
    /// Origin at which this browser session was paired.
    pub origin: String,
    /// Effective session expiration after idle and absolute limits.
    pub expires_at: Timestamp,
}

/// Newly created browser session and its raw cookie token.
#[derive(Debug)]
pub struct CreatedBrowserSession {
    /// Stable browser session identifier.
    pub id: api::BrowserSessionId,
    /// Opaque value stored in the `HttpOnly` session cookie.
    pub cookie_token: String,
    /// Effective session expiration.
    pub expires_at: Timestamp,
}

/// Newly created route grant and its raw cookie token.
#[derive(Debug)]
pub struct CreatedRouteGrant {
    /// Opaque value stored in the route-specific `HttpOnly` cookie.
    pub cookie_token: String,
    /// Time after which the route grant is rejected.
    pub expires_at: Timestamp,
    /// Clean target restored after the bridge exchange.
    pub return_to: String,
    /// Parent browser session authorizing the route.
    pub session_id: api::BrowserSessionId,
    /// Route authorized by the grant.
    pub route_id: HttpRouteId,
}

/// Subscription to complete browser-session snapshots.
pub struct BrowserSessionSubscription {
    service: AuthService,
    changes: watch::Receiver<u64>,
    current_session_id: Option<api::BrowserSessionId>,
    initial: bool,
}

/// RAII accounting for one authenticated control WebSocket.
pub struct BrowserConnectionGuard {
    service: AuthService,
    session_id: api::BrowserSessionId,
}

impl AuthService {
    /// Opens durable authentication state and creates its private key if
    /// absent.
    ///
    /// # Errors
    ///
    /// Returns an error when the key or `SQLite` state cannot be opened safely.
    #[tracing::instrument(level = "debug", skip_all, err)]
    pub async fn open(config: AuthServiceConfig) -> Result<Self, Report<AuthServiceError>> {
        validate_config(&config)?;
        create_private_directory(&config.state_directory)
            .map_err(|source| unavailable("prepare authentication state directory", source))?;
        let secret_path = config
            .secret_file
            .clone()
            .unwrap_or_else(|| config.state_directory.join(AUTH_SECRET_FILE));
        let secret = load_or_create_secret(&secret_path, config.secret_file.is_none())?;
        let storage = AuthStorage::open(config.state_directory.join(AUTH_DATABASE_FILE)).await?;
        let (changes, _) = watch::channel(0);
        Ok(Self {
            inner: Arc::new(AuthServiceInner {
                storage,
                secret,
                config,
                pending: Mutex::new(PendingCapabilities::default()),
                active_connections: Mutex::new(HashMap::new()),
                changes,
            }),
        })
    }

    /// Creates a local-admin pairing key.
    ///
    /// # Errors
    ///
    /// Returns an error when the label is invalid, the in-memory capability
    /// limit is exhausted, or the expiration cannot be represented.
    #[tracing::instrument(level = "debug", skip_all, err)]
    pub fn create_pairing_key(
        &self,
        label: Option<String>,
    ) -> Result<api::CreatePairingKeyOutput, Report<AuthServiceError>> {
        let label = label.as_deref().map(validate_label).transpose()?;
        let now = Timestamp::now();
        let expires_at = add_duration(now, self.inner.config.pairing_lifetime)?;
        let token = random_token();
        let digest = self.digest(b"pairing", token.as_bytes());
        let mut pending = lock(&self.inner.pending);
        prune_pending(&mut pending, now);
        if pending.pairing_keys.len() >= self.inner.config.max_pending_pairing_keys {
            return Err(AuthServiceError::Capacity.report());
        }
        pending
            .pairing_keys
            .insert(digest, PendingPairingKey { expires_at, label });
        Ok(api::CreatePairingKeyOutput {
            pairing_key: format!("tascarrel-pair-{token}").into(),
            expires_at,
        })
    }

    /// Exchanges one pairing key for a durable browser session.
    ///
    /// # Errors
    ///
    /// Returns an error when the pairing capability or label is invalid, the
    /// session expiration cannot be represented, or durable state is
    /// unavailable.
    #[tracing::instrument(level = "debug", skip_all, err)]
    pub async fn pair_browser(
        &self,
        pairing_key: &str,
        label: Option<&str>,
        origin: &str,
    ) -> Result<CreatedBrowserSession, Report<AuthServiceError>> {
        let requested_label = label
            .map(str::trim)
            .filter(|label| !label.is_empty())
            .map(validate_label)
            .transpose()?;
        let token = pairing_key
            .strip_prefix("tascarrel-pair-")
            .ok_or_else(|| AuthServiceError::InvalidPairingKey.report())?;
        let digest = self.digest(b"pairing", token.as_bytes());
        let pending = {
            let mut state = lock(&self.inner.pending);
            prune_pending(&mut state, Timestamp::now());
            state
                .pairing_keys
                .remove(&digest)
                .ok_or_else(|| AuthServiceError::InvalidPairingKey.report())?
        };
        let label = requested_label
            .or(pending.label)
            .unwrap_or_else(|| "Browser".to_owned());
        let now = Timestamp::now();
        if pending.expires_at <= now {
            return Err(AuthServiceError::InvalidPairingKey.report());
        }
        let idle_expires_at = add_duration(now, self.inner.config.idle_session_lifetime)?;
        let absolute_expires_at = add_duration(now, self.inner.config.absolute_session_lifetime)?;
        let expires_at = idle_expires_at.min(absolute_expires_at);
        let id = api::BrowserSessionId::generate();
        let secret = random_token();
        let token_digest = self.digest(b"session", secret.as_bytes());
        self.inner
            .storage
            .insert_session(NewBrowserSession {
                id: id.clone(),
                token_hmac: token_digest,
                label,
                origin: origin.to_owned(),
                created_at: now,
                idle_expires_at,
                absolute_expires_at,
            })
            .await?;
        self.notify_changed();
        Ok(CreatedBrowserSession {
            cookie_token: format!("{}.{secret}", id.0),
            id,
            expires_at,
        })
    }

    /// Authenticates and touches one durable browser cookie.
    ///
    /// # Errors
    ///
    /// Returns an error when the cookie is invalid or durable authentication
    /// state is unavailable.
    #[tracing::instrument(level = "trace", skip_all, err)]
    pub async fn authenticate_browser(
        &self,
        cookie_token: &str,
    ) -> Result<AuthenticatedSession, Report<AuthServiceError>> {
        let (id, secret) = parse_cookie_token::<api::BrowserSessionId>(cookie_token)?;
        let token_digest = self.digest(b"session", secret.as_bytes());
        let now = Timestamp::now();
        let session = self
            .inner
            .storage
            .authenticate_session(&id, &token_digest, self.session_activity(now))
            .await?
            .ok_or_else(|| AuthServiceError::InvalidSession.report())?;
        if session.touched {
            self.notify_changed();
        }
        Ok(AuthenticatedSession {
            id,
            origin: session.origin,
            expires_at: session.expires_at,
        })
    }

    /// Revokes one browser session and all route grants derived from it.
    ///
    /// # Errors
    ///
    /// Returns an error when durable authentication state cannot be updated.
    #[tracing::instrument(level = "debug", skip_all, err)]
    pub async fn revoke_session(
        &self,
        id: &api::BrowserSessionId,
    ) -> Result<(), Report<AuthServiceError>> {
        self.inner.storage.revoke_session(id).await?;
        self.notify_changed();
        Ok(())
    }

    /// Returns whether a browser session remains usable.
    ///
    /// # Errors
    ///
    /// Returns an error when durable authentication state cannot be queried.
    pub async fn session_is_active(
        &self,
        id: &api::BrowserSessionId,
    ) -> Result<bool, Report<AuthServiceError>> {
        self.inner
            .storage
            .session_is_active(id, Timestamp::now())
            .await
    }

    /// Extends the idle lifetime of an already authenticated active browser.
    ///
    /// # Errors
    ///
    /// Returns an error when the updated expiration cannot be represented or
    /// durable authentication state cannot be updated.
    pub async fn keep_browser_alive(
        &self,
        id: &api::BrowserSessionId,
    ) -> Result<bool, Report<AuthServiceError>> {
        let (active, touched) = self
            .inner
            .storage
            .keep_session_alive(id, self.session_activity(Timestamp::now()))
            .await?;
        if touched {
            self.notify_changed();
        }
        Ok(active)
    }

    /// Creates a single-use route ticket for an exact host-issued route.
    ///
    /// # Errors
    ///
    /// Returns an error when the return target is invalid, the in-memory
    /// capability limit is exhausted, or the expiration cannot be represented.
    #[tracing::instrument(level = "debug", skip_all, err)]
    pub fn create_route_ticket(
        &self,
        session_id: api::BrowserSessionId,
        route_id: HttpRouteId,
        hostname: String,
        return_to: String,
    ) -> Result<String, Report<AuthServiceError>> {
        validate_return_to(&return_to)?;
        let now = Timestamp::now();
        let expires_at = add_duration(now, self.inner.config.route_ticket_lifetime)?;
        let token = random_token();
        let digest = self.digest(b"route-ticket", token.as_bytes());
        let mut pending = lock(&self.inner.pending);
        prune_pending(&mut pending, now);
        if pending.route_tickets.len() >= self.inner.config.max_pending_route_tickets {
            return Err(AuthServiceError::Capacity.report());
        }
        pending.route_tickets.insert(
            digest,
            PendingRouteTicket {
                session_id,
                route_id,
                hostname,
                return_to,
                expires_at,
            },
        );
        Ok(format!("tascarrel-route-{token}"))
    }

    /// Consumes a route ticket and creates a durable route cookie grant.
    ///
    /// # Errors
    ///
    /// Returns an error when the ticket or parent browser session is invalid,
    /// the expiration cannot be represented, or durable state is unavailable.
    #[tracing::instrument(level = "debug", skip_all, err)]
    pub async fn exchange_route_ticket(
        &self,
        ticket: &str,
        hostname: &str,
    ) -> Result<CreatedRouteGrant, Report<AuthServiceError>> {
        let token = ticket
            .strip_prefix("tascarrel-route-")
            .ok_or_else(|| AuthServiceError::InvalidRouteTicket.report())?;
        let digest = self.digest(b"route-ticket", token.as_bytes());
        let pending = {
            let mut state = lock(&self.inner.pending);
            prune_pending(&mut state, Timestamp::now());
            state
                .route_tickets
                .remove(&digest)
                .ok_or_else(|| AuthServiceError::InvalidRouteTicket.report())?
        };
        let now = Timestamp::now();
        if pending.expires_at <= now || !pending.hostname.eq_ignore_ascii_case(hostname) {
            return Err(AuthServiceError::InvalidRouteTicket.report());
        }
        if !self.session_is_active(&pending.session_id).await? {
            return Err(AuthServiceError::InvalidSession.report());
        }
        let expires_at = add_duration(now, self.inner.config.route_grant_lifetime)?;
        let grant_id = random_token();
        let secret = random_token();
        let token_digest = self.digest(b"route-grant", secret.as_bytes());
        self.inner
            .storage
            .insert_route_grant(NewRouteGrant {
                id: grant_id.clone(),
                session_id: pending.session_id.clone(),
                route_id: pending.route_id.clone(),
                hostname: hostname.to_owned(),
                token_hmac: token_digest,
                created_at: now,
                expires_at,
            })
            .await?;
        Ok(CreatedRouteGrant {
            cookie_token: format!("{grant_id}.{secret}"),
            expires_at,
            return_to: pending.return_to,
            session_id: pending.session_id,
            route_id: pending.route_id,
        })
    }

    /// Authenticates a route-specific cookie for one exact route.
    ///
    /// # Errors
    ///
    /// Returns an error when the route grant is invalid or durable
    /// authentication state is unavailable.
    #[tracing::instrument(level = "trace", skip_all, err)]
    pub async fn authenticate_route(
        &self,
        cookie_token: &str,
        route_id: &HttpRouteId,
        hostname: &str,
    ) -> Result<AuthenticatedSession, Report<AuthServiceError>> {
        let (grant_id, secret) = parse_raw_cookie_token(cookie_token)?;
        let token_digest = self.digest(b"route-grant", secret.as_bytes());
        let route_session = self
            .inner
            .storage
            .authenticate_route_grant(
                &grant_id,
                route_id,
                hostname,
                &token_digest,
                self.session_activity(Timestamp::now()),
            )
            .await?
            .ok_or_else(|| AuthServiceError::InvalidRouteGrant.report())?;
        if route_session.touched {
            self.notify_changed();
        }
        Ok(route_session.session)
    }

    /// Subscribes to complete browser-session inventories.
    #[must_use]
    pub fn subscribe_sessions(
        &self,
        current_session_id: Option<api::BrowserSessionId>,
    ) -> BrowserSessionSubscription {
        BrowserSessionSubscription {
            service: self.clone(),
            changes: self.inner.changes.subscribe(),
            current_session_id,
            initial: true,
        }
    }

    /// Returns the configured maximum browser-session lifetime.
    #[must_use]
    pub(crate) fn absolute_session_lifetime(&self) -> Duration {
        self.inner.config.absolute_session_lifetime
    }

    /// Returns the configured route-grant lifetime.
    #[must_use]
    pub(crate) fn route_grant_lifetime(&self) -> Duration {
        self.inner.config.route_grant_lifetime
    }

    /// Records one connected browser control socket.
    #[must_use]
    pub fn connect_browser(&self, session_id: api::BrowserSessionId) -> BrowserConnectionGuard {
        let mut connections = lock(&self.inner.active_connections);
        let count = connections.entry(session_id.clone()).or_default();
        *count = count.saturating_add(1);
        drop(connections);
        self.notify_changed();
        BrowserConnectionGuard {
            service: self.clone(),
            session_id,
        }
    }

    fn digest(&self, domain: &[u8], token: &[u8]) -> [u8; 32] {
        let mut mac = HmacSha256::new_from_slice(&self.inner.secret)
            .expect("HMAC accepts every 256-bit authentication key");
        mac.update(domain);
        mac.update(&[0]);
        mac.update(token);
        mac.finalize().into_bytes().into()
    }

    /// Captures the configured durable-session refresh policy at `now`.
    fn session_activity(&self, now: Timestamp) -> SessionActivity {
        SessionActivity {
            now,
            idle_lifetime: self.inner.config.idle_session_lifetime,
            touch_interval: self.inner.config.session_touch_interval,
        }
    }

    fn notify_changed(&self) {
        let next = self.inner.changes.borrow().wrapping_add(1);
        self.inner.changes.send_replace(next);
    }
}

impl BrowserSessionSubscription {
    /// Receives the initial browser-session snapshot or the next changed one.
    ///
    /// # Errors
    ///
    /// Returns an error when the change channel closes or durable
    /// authentication state cannot be queried.
    pub async fn recv(
        &mut self,
    ) -> Result<api::BrowserSessionsChangedEvent, Report<AuthServiceError>> {
        if self.initial {
            self.initial = false;
        } else if self.changes.changed().await.is_err() {
            return Err(AuthServiceError::Unavailable.report());
        }
        let active_connections = lock(&self.service.inner.active_connections).clone();
        let sessions = self
            .service
            .inner
            .storage
            .list_sessions(Timestamp::now(), &active_connections)
            .await?;
        Ok(api::BrowserSessionsChangedEvent {
            sessions: sessions.into(),
            current_session_id: self.current_session_id.clone(),
        })
    }
}

impl Drop for BrowserConnectionGuard {
    fn drop(&mut self) {
        let mut connections = lock(&self.service.inner.active_connections);
        if let Some(count) = connections.get_mut(&self.session_id) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                connections.remove(&self.session_id);
            }
        }
        drop(connections);
        self.service.notify_changed();
    }
}

/// Authentication service failures safe to classify at API boundaries.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum AuthServiceError {
    /// Authentication state could not be opened or updated.
    #[error("authentication service is unavailable")]
    Unavailable,
    /// The pairing key is unknown, expired, or already consumed.
    #[error("pairing key is invalid or expired")]
    InvalidPairingKey,
    /// The browser session is unknown, expired, or revoked.
    #[error("browser session is invalid or expired")]
    InvalidSession,
    /// The route ticket is unknown, expired, already consumed, or misdirected.
    #[error("HTTP route ticket is invalid or expired")]
    InvalidRouteTicket,
    /// The route cookie is invalid, expired, or scoped to another route.
    #[error("HTTP route grant is invalid or expired")]
    InvalidRouteGrant,
    /// The requested authentication operation is invalid.
    #[error("authentication request is invalid")]
    InvalidRequest,
    /// Too many pending authentication capabilities exist.
    #[error("authentication capability capacity is exhausted")]
    Capacity,
}

fn validate_config(config: &AuthServiceConfig) -> Result<(), Report<AuthServiceError>> {
    let lifetimes = [
        config.pairing_lifetime,
        config.route_ticket_lifetime,
        config.idle_session_lifetime,
        config.absolute_session_lifetime,
        config.session_touch_interval,
        config.route_grant_lifetime,
    ];
    if lifetimes
        .into_iter()
        .any(|lifetime| lifetime.as_secs() == 0 || lifetime.as_secs() > i64::MAX as u64)
        || config.session_touch_interval >= config.idle_session_lifetime
        || config.max_pending_pairing_keys == 0
        || config.max_pending_route_tickets == 0
    {
        return Err(AuthServiceError::InvalidRequest
            .report()
            .message("authentication lifetimes or capacity limits are invalid"));
    }
    Ok(())
}

fn validate_label(label: &str) -> Result<String, Report<AuthServiceError>> {
    let label = label.trim();
    if label.is_empty()
        || label.len() > MAX_SESSION_LABEL_BYTES
        || label.chars().any(char::is_control)
    {
        return Err(AuthServiceError::InvalidRequest
            .report()
            .message("session label must contain 1-80 bytes without control characters"));
    }
    Ok(label.to_owned())
}

fn validate_return_to(return_to: &str) -> Result<(), Report<AuthServiceError>> {
    if !return_to.starts_with('/')
        || return_to.starts_with("//")
        || return_to.contains('\\')
        || return_to.chars().any(char::is_control)
    {
        return Err(AuthServiceError::InvalidRequest
            .report()
            .message("route return target must be an absolute path"));
    }
    Ok(())
}

fn load_or_create_secret(
    path: &Path,
    create: bool,
) -> Result<[u8; AUTH_SECRET_BYTES], Report<AuthServiceError>> {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .custom_flags(nix::libc::O_CLOEXEC | nix::libc::O_NOFOLLOW);
    match options.open(path) {
        Ok(mut file) => read_secret(&mut file),
        Err(error) if create && error.kind() == std::io::ErrorKind::NotFound => create_secret(path),
        Err(source) => Err(unavailable("open authentication key", source)),
    }
}

fn read_secret(
    file: &mut std::fs::File,
) -> Result<[u8; AUTH_SECRET_BYTES], Report<AuthServiceError>> {
    let metadata = file
        .metadata()
        .map_err(|source| unavailable("inspect authentication key", source))?;
    if !metadata.is_file()
        || metadata.len() != AUTH_SECRET_BYTES as u64
        || metadata.permissions().mode() & 0o077 != 0
    {
        return Err(AuthServiceError::Unavailable
            .report()
            .message("authentication key must be a private 32-byte regular file"));
    }
    let mut secret = [0_u8; AUTH_SECRET_BYTES];
    file.read_exact(&mut secret)
        .map_err(|source| unavailable("read authentication key", source))?;
    Ok(secret)
}

fn create_secret(path: &Path) -> Result<[u8; AUTH_SECRET_BYTES], Report<AuthServiceError>> {
    let mut secret = [0_u8; AUTH_SECRET_BYTES];
    rand::rng().fill(&mut secret);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(nix::libc::O_CLOEXEC | nix::libc::O_NOFOLLOW)
        .open(path)
        .map_err(|source| unavailable("create authentication key", source))?;
    file.write_all(&secret)
        .map_err(|source| unavailable("write authentication key", source))?;
    file.sync_all()
        .map_err(|source| unavailable("persist authentication key", source))?;
    Ok(secret)
}

fn parse_cookie_token<I>(value: &str) -> Result<(I, &str), Report<AuthServiceError>>
where
    I: FromStr,
    I::Err: std::fmt::Display,
{
    let (id, secret) = value
        .split_once('.')
        .ok_or_else(|| AuthServiceError::InvalidSession.report())?;
    let id = id
        .parse()
        .map_err(|_| AuthServiceError::InvalidSession.report())?;
    validate_opaque_secret(secret).map_err(|()| AuthServiceError::InvalidSession.report())?;
    Ok((id, secret))
}

fn parse_raw_cookie_token(value: &str) -> Result<(String, &str), Report<AuthServiceError>> {
    let (id, secret) = value
        .split_once('.')
        .ok_or_else(|| AuthServiceError::InvalidRouteGrant.report())?;
    validate_opaque_secret(id).map_err(|()| AuthServiceError::InvalidRouteGrant.report())?;
    validate_opaque_secret(secret).map_err(|()| AuthServiceError::InvalidRouteGrant.report())?;
    Ok((id.to_owned(), secret))
}

fn validate_opaque_secret(value: &str) -> Result<(), ()> {
    let decoded = URL_SAFE_NO_PAD.decode(value).map_err(|_| ())?;
    if decoded.len() != OPAQUE_TOKEN_BYTES {
        return Err(());
    }
    Ok(())
}

fn random_token() -> String {
    let mut bytes = [0_u8; OPAQUE_TOKEN_BYTES];
    rand::rng().fill(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

fn add_duration(
    timestamp: Timestamp,
    duration: Duration,
) -> Result<Timestamp, Report<AuthServiceError>> {
    timestamp.checked_add(duration).map_err(|source| {
        AuthServiceError::Unavailable
            .report()
            .message(source.to_string())
    })
}

fn prune_pending(pending: &mut PendingCapabilities, now: Timestamp) {
    pending
        .pairing_keys
        .retain(|_, pairing| pairing.expires_at > now);
    pending
        .route_tickets
        .retain(|_, ticket| ticket.expires_at > now);
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            warn!(error = %poisoned, "authentication state lock was poisoned");
            poisoned.into_inner()
        }
    }
}

fn unavailable(
    operation: &'static str,
    source: impl std::fmt::Display,
) -> Report<AuthServiceError> {
    AuthServiceError::Unavailable
        .report()
        .message(format!("{operation}: {source}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn service(directory: &Path) -> AuthService {
        AuthService::open(AuthServiceConfig::new(directory))
            .await
            .expect("open authentication service")
    }

    /// Verifies pairing capabilities are single-use and the resulting
    /// credentials remain valid after reopening durable state.
    #[tokio::test]
    async fn pairing_keys_are_single_use_and_sessions_survive_restart() {
        let directory = tempfile::tempdir().unwrap();
        let auth = service(directory.path()).await;
        let key = auth
            .create_pairing_key(Some("Laptop".to_owned()))
            .unwrap()
            .pairing_key;
        let created = auth
            .pair_browser(key.as_ref(), None, "https://tascarrel.example")
            .await
            .unwrap();

        assert!(
            auth.pair_browser(key.as_ref(), None, "https://tascarrel.example")
                .await
                .is_err()
        );
        let authenticated = auth
            .authenticate_browser(&created.cookie_token)
            .await
            .unwrap();
        assert_eq!(authenticated.id, created.id);
        assert_eq!(authenticated.origin, "https://tascarrel.example");

        drop(auth);
        let reopened = service(directory.path()).await;
        assert_eq!(
            reopened
                .authenticate_browser(&created.cookie_token)
                .await
                .unwrap()
                .id,
            created.id
        );
    }

    /// Verifies route credentials cannot cross route boundaries and parent
    /// session revocation invalidates every derived grant.
    #[tokio::test]
    async fn route_grants_are_scoped_and_revoked_with_the_parent_session() {
        let directory = tempfile::tempdir().unwrap();
        let auth = service(directory.path()).await;
        let key = auth.create_pairing_key(None).unwrap().pairing_key;
        let browser = auth
            .pair_browser(key.as_ref(), Some("Browser"), "http://localhost:8272")
            .await
            .unwrap();
        let route_id = HttpRouteId::generate();
        let ticket = auth
            .create_route_ticket(
                browser.id.clone(),
                route_id.clone(),
                "route.tascarrel.localhost".to_owned(),
                "/preview?theme=dark#editor".to_owned(),
            )
            .unwrap();
        let grant = auth
            .exchange_route_ticket(&ticket, "route.tascarrel.localhost")
            .await
            .unwrap();

        assert_eq!(grant.return_to, "/preview?theme=dark#editor");
        assert!(
            auth.exchange_route_ticket(&ticket, "route.tascarrel.localhost")
                .await
                .is_err()
        );
        assert!(
            auth.authenticate_route(
                &grant.cookie_token,
                &HttpRouteId::generate(),
                "route.tascarrel.localhost",
            )
            .await
            .is_err()
        );
        assert_eq!(
            auth.authenticate_route(&grant.cookie_token, &route_id, "route.tascarrel.localhost",)
                .await
                .unwrap()
                .id,
            browser.id
        );

        auth.revoke_session(&browser.id).await.unwrap();
        assert!(
            auth.authenticate_route(&grant.cookie_token, &route_id, "route.tascarrel.localhost",)
                .await
                .is_err()
        );
    }

    /// Verifies a contract error does not consume an otherwise valid
    /// single-use pairing capability.
    #[tokio::test]
    async fn invalid_pairing_labels_do_not_consume_the_key() {
        let directory = tempfile::tempdir().unwrap();
        let auth = service(directory.path()).await;
        let key = auth.create_pairing_key(None).unwrap().pairing_key;

        assert!(
            auth.pair_browser(key.as_ref(), Some("bad\nlabel"), "http://localhost:8272",)
                .await
                .is_err()
        );
        assert!(
            auth.pair_browser(key.as_ref(), Some("Laptop"), "http://localhost:8272")
                .await
                .is_ok()
        );
    }
}
