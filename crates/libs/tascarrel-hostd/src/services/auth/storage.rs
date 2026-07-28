//! `SQLite` persistence for browser sessions and route grants.
//!
//! [`AuthStorage`] serializes durable credential checks and session activity
//! updates on one asynchronous database connection owned by
//! [`super::AuthService`].

use std::collections::HashMap;
use std::path::Path;
use std::str::FromStr as _;
use std::time::Duration;

use jiff::Timestamp;
use reportify::ErrorExt as _;
use reportify::Report;
use subtle::ConstantTimeEq as _;
use tascarrel_api::types::auth as api;
use tascarrel_api::types::network::HttpRouteId;
use tokio_rusqlite::Connection;
use tokio_rusqlite::rusqlite;

use super::AuthServiceError;
use super::AuthenticatedSession;
use super::add_duration;

const DEFAULT_DATABASE_BUSY_TIMEOUT: Duration = Duration::from_secs(5);

const CONNECTION_CONFIGURATION: &str = r"
PRAGMA foreign_keys = ON;
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA trusted_schema = OFF;
";

const SCHEMA: &str = r"
CREATE TABLE IF NOT EXISTS browser_sessions (
    id TEXT PRIMARY KEY,
    token_hmac BLOB NOT NULL CHECK (length(token_hmac) = 32),
    label TEXT NOT NULL,
    origin TEXT NOT NULL,
    created_at TEXT NOT NULL,
    last_seen_at TEXT NOT NULL,
    idle_expires_at TEXT NOT NULL,
    absolute_expires_at TEXT NOT NULL,
    revoked_at TEXT
) STRICT;

CREATE TABLE IF NOT EXISTS route_grants (
    id TEXT PRIMARY KEY,
    browser_session_id TEXT NOT NULL REFERENCES browser_sessions(id) ON DELETE CASCADE,
    http_route_id TEXT NOT NULL,
    hostname TEXT NOT NULL,
    token_hmac BLOB NOT NULL CHECK (length(token_hmac) = 32),
    created_at TEXT NOT NULL,
    expires_at TEXT NOT NULL
) STRICT;

CREATE INDEX IF NOT EXISTS route_grants_session
ON route_grants(browser_session_id);
";

/// Durable authentication database accessed through one asynchronous
/// connection worker.
#[derive(Clone)]
pub(crate) struct AuthStorage {
    connection: Connection,
}

/// Browser-session metadata returned after a successful credential check.
pub(crate) struct AuthenticatedStorageSession {
    /// Effective idle or absolute expiration, whichever occurs first.
    pub(crate) expires_at: Timestamp,
    /// Canonical origin recorded when the browser paired.
    pub(crate) origin: String,
    /// Whether authentication advanced the durable idle lifetime.
    pub(crate) touched: bool,
}

/// Parent browser session returned after a successful route-grant check.
pub(crate) struct AuthenticatedRouteStorageSession {
    /// Browser session from which the route grant was delegated.
    pub(crate) session: AuthenticatedSession,
    /// Whether route access advanced the browser's durable idle lifetime.
    pub(crate) touched: bool,
}

/// Values used to validate and refresh one active browser session.
#[derive(Clone, Copy)]
pub(crate) struct SessionActivity {
    /// Current wall-clock time for this authentication attempt.
    pub(crate) now: Timestamp,
    /// Idle lifetime restored by a durable activity update.
    pub(crate) idle_lifetime: Duration,
    /// Minimum interval between durable activity updates.
    pub(crate) touch_interval: Duration,
}

/// Durable values for one newly paired browser session.
pub(crate) struct NewBrowserSession {
    /// Stable browser-session identifier.
    pub(crate) id: api::BrowserSessionId,
    /// Keyed digest of the opaque browser credential.
    pub(crate) token_hmac: [u8; 32],
    /// User-facing browser or device label.
    pub(crate) label: String,
    /// Browser origin on which the session was established.
    pub(crate) origin: String,
    /// Session creation time.
    pub(crate) created_at: Timestamp,
    /// Initial idle expiration.
    pub(crate) idle_expires_at: Timestamp,
    /// Non-renewable session expiration.
    pub(crate) absolute_expires_at: Timestamp,
}

/// Durable values for one newly delegated HTTP route grant.
pub(crate) struct NewRouteGrant {
    /// Opaque grant identifier.
    pub(crate) id: String,
    /// Browser session delegating route access.
    pub(crate) session_id: api::BrowserSessionId,
    /// Exact host-issued route receiving access.
    pub(crate) route_id: HttpRouteId,
    /// Exact route hostname receiving the cookie.
    pub(crate) hostname: String,
    /// Keyed digest of the opaque route credential.
    pub(crate) token_hmac: [u8; 32],
    /// Route-grant creation time.
    pub(crate) created_at: Timestamp,
    /// Non-renewable route-grant expiration.
    pub(crate) expires_at: Timestamp,
}

impl AuthStorage {
    /// Opens the database and initializes its strict authentication schema.
    pub(crate) async fn open(path: impl AsRef<Path>) -> Result<Self, Report<AuthServiceError>> {
        let path = path.as_ref().to_owned();
        let connection = Connection::open(&path)
            .await
            .map_err(|source| unavailable("open authentication database", source))?;
        connection
            .call_raw(|database| {
                database
                    .execute_batch(CONNECTION_CONFIGURATION)
                    .map_err(|source| unavailable("configure authentication database", source))?;
                database
                    .busy_timeout(DEFAULT_DATABASE_BUSY_TIMEOUT)
                    .map_err(|source| {
                        unavailable("configure authentication busy timeout", source)
                    })?;
                database
                    .execute_batch(SCHEMA)
                    .map_err(|source| unavailable("initialize authentication database", source))
            })
            .await
            .map_err(|source| unavailable("access authentication database", source))??;
        Ok(Self { connection })
    }

    /// Inserts one newly paired browser session.
    pub(crate) async fn insert_session(
        &self,
        session: NewBrowserSession,
    ) -> Result<(), Report<AuthServiceError>> {
        let id = session.id.0.to_string();
        let token_hmac = session.token_hmac.to_vec();
        self.call("insert browser session", move |database| {
            database
                .execute(
                    r"
INSERT INTO browser_sessions (
    id, token_hmac, label, origin, created_at, last_seen_at,
    idle_expires_at, absolute_expires_at, revoked_at
) VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?6, ?7, NULL)
",
                    rusqlite::params![
                        id,
                        token_hmac,
                        session.label,
                        session.origin,
                        session.created_at.to_string(),
                        session.idle_expires_at.to_string(),
                        session.absolute_expires_at.to_string(),
                    ],
                )
                .map_err(|source| unavailable("insert browser session", source))?;
            Ok(())
        })
        .await
    }

    /// Verifies a browser token and periodically advances its idle lifetime.
    pub(crate) async fn authenticate_session(
        &self,
        id: &api::BrowserSessionId,
        token_hmac: &[u8; 32],
        activity: SessionActivity,
    ) -> Result<Option<AuthenticatedStorageSession>, Report<AuthServiceError>> {
        let id = id.0.to_string();
        let token_hmac = token_hmac.to_vec();
        self.call("authenticate browser session", move |database| {
            let row = database
                .query_row(
                    r"
SELECT token_hmac, origin, last_seen_at, idle_expires_at, absolute_expires_at
FROM browser_sessions
WHERE id = ?1 AND revoked_at IS NULL
",
                    [&id],
                    |row| {
                        Ok((
                            row.get::<_, Vec<u8>>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                            row.get::<_, String>(4)?,
                        ))
                    },
                )
                .optional()
                .map_err(|source| unavailable("query browser session", source))?;
            let Some((stored_hmac, origin, last_seen, idle_expires, absolute_expires)) = row else {
                return Ok(None);
            };
            if !bool::from(stored_hmac.ct_eq(token_hmac.as_slice())) {
                return Ok(None);
            }
            let last_seen = parse_timestamp(&last_seen, "last_seen_at")?;
            let idle_expires = parse_timestamp(&idle_expires, "idle_expires_at")?;
            let absolute_expires = parse_timestamp(&absolute_expires, "absolute_expires_at")?;
            if idle_expires <= activity.now || absolute_expires <= activity.now {
                return Ok(None);
            }
            let touch_after = add_duration(last_seen, activity.touch_interval)?;
            let touched = activity.now >= touch_after;
            let expires_at = if touched {
                let updated_idle =
                    add_duration(activity.now, activity.idle_lifetime)?.min(absolute_expires);
                database
                    .execute(
                        r"
UPDATE browser_sessions
SET last_seen_at = ?2, idle_expires_at = ?3
WHERE id = ?1 AND revoked_at IS NULL
",
                        rusqlite::params![id, activity.now.to_string(), updated_idle.to_string()],
                    )
                    .map_err(|source| unavailable("touch browser session", source))?;
                updated_idle
            } else {
                idle_expires.min(absolute_expires)
            };
            Ok(Some(AuthenticatedStorageSession {
                expires_at,
                origin,
                touched,
            }))
        })
        .await
    }

    /// Returns whether an identified browser remains active.
    pub(crate) async fn session_is_active(
        &self,
        id: &api::BrowserSessionId,
        now: Timestamp,
    ) -> Result<bool, Report<AuthServiceError>> {
        let id = id.0.to_string();
        self.call("inspect browser session", move |database| {
            let row = database
                .query_row(
                    r"
SELECT idle_expires_at, absolute_expires_at
FROM browser_sessions
WHERE id = ?1 AND revoked_at IS NULL
",
                    [&id],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                )
                .optional()
                .map_err(|source| unavailable("query browser session activity", source))?;
            let Some((idle_expires, absolute_expires)) = row else {
                return Ok(false);
            };
            Ok(parse_timestamp(&idle_expires, "idle_expires_at")? > now
                && parse_timestamp(&absolute_expires, "absolute_expires_at")? > now)
        })
        .await
    }

    /// Periodically advances the idle lifetime of a connected browser.
    pub(crate) async fn keep_session_alive(
        &self,
        id: &api::BrowserSessionId,
        activity: SessionActivity,
    ) -> Result<(bool, bool), Report<AuthServiceError>> {
        let id = id.0.to_string();
        self.call("keep browser session alive", move |database| {
            let row = database
                .query_row(
                    r"
SELECT last_seen_at, idle_expires_at, absolute_expires_at
FROM browser_sessions
WHERE id = ?1 AND revoked_at IS NULL
",
                    [&id],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                        ))
                    },
                )
                .optional()
                .map_err(|source| unavailable("query active browser session", source))?;
            let Some((last_seen, idle_expires, absolute_expires)) = row else {
                return Ok((false, false));
            };
            let last_seen = parse_timestamp(&last_seen, "last_seen_at")?;
            let idle_expires = parse_timestamp(&idle_expires, "idle_expires_at")?;
            let absolute_expires = parse_timestamp(&absolute_expires, "absolute_expires_at")?;
            if idle_expires <= activity.now || absolute_expires <= activity.now {
                return Ok((false, false));
            }
            let touch_after = add_duration(last_seen, activity.touch_interval)?;
            if activity.now < touch_after {
                return Ok((true, false));
            }
            let updated_idle =
                add_duration(activity.now, activity.idle_lifetime)?.min(absolute_expires);
            database
                .execute(
                    r"
UPDATE browser_sessions
SET last_seen_at = ?2, idle_expires_at = ?3
WHERE id = ?1 AND revoked_at IS NULL
",
                    rusqlite::params![id, activity.now.to_string(), updated_idle.to_string()],
                )
                .map_err(|source| unavailable("touch active browser session", source))?;
            Ok((true, true))
        })
        .await
    }

    /// Marks one browser session as revoked.
    pub(crate) async fn revoke_session(
        &self,
        id: &api::BrowserSessionId,
    ) -> Result<(), Report<AuthServiceError>> {
        let id = id.0.to_string();
        let now = Timestamp::now().to_string();
        self.call("revoke browser session", move |database| {
            database
                .execute(
                    "UPDATE browser_sessions SET revoked_at = ?2 WHERE id = ?1 AND revoked_at IS NULL",
                    rusqlite::params![id, now],
                )
                .map_err(|source| unavailable("revoke browser session", source))?;
            Ok(())
        })
        .await
    }

    /// Returns every non-expired, non-revoked browser session.
    pub(crate) async fn list_sessions(
        &self,
        now: Timestamp,
        active_connections: &HashMap<api::BrowserSessionId, u32>,
    ) -> Result<Vec<api::BrowserSession>, Report<AuthServiceError>> {
        let active_connections = active_connections.clone();
        self.call("list browser sessions", move |database| {
            let mut statement = database
                .prepare(
                    r"
SELECT id, label, origin, created_at, last_seen_at,
       idle_expires_at, absolute_expires_at
FROM browser_sessions
WHERE revoked_at IS NULL
ORDER BY created_at DESC, id
",
                )
                .map_err(|source| unavailable("prepare browser session list", source))?;
            let rows = statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, String>(6)?,
                    ))
                })
                .map_err(|source| unavailable("query browser sessions", source))?;
            let mut sessions = Vec::new();
            for row in rows {
                let (id, label, origin, created_at, last_seen_at, idle_expires, absolute_expires) =
                    row.map_err(|source| unavailable("decode browser session row", source))?;
                let id = api::BrowserSessionId::from_str(&id)
                    .map_err(|source| unavailable("decode browser session identifier", source))?;
                let idle_expires = parse_timestamp(&idle_expires, "idle_expires_at")?;
                let absolute_expires = parse_timestamp(&absolute_expires, "absolute_expires_at")?;
                let expires_at = idle_expires.min(absolute_expires);
                if expires_at <= now {
                    continue;
                }
                sessions.push(api::BrowserSession {
                    active_connections: active_connections.get(&id).copied().unwrap_or(0),
                    id,
                    label: label.into(),
                    origin: origin.into(),
                    created_at: parse_timestamp(&created_at, "created_at")?,
                    last_seen_at: parse_timestamp(&last_seen_at, "last_seen_at")?,
                    expires_at,
                });
            }
            Ok(sessions)
        })
        .await
    }

    /// Inserts a route grant delegated from one browser session.
    pub(crate) async fn insert_route_grant(
        &self,
        grant: NewRouteGrant,
    ) -> Result<(), Report<AuthServiceError>> {
        let session_id = grant.session_id.0.to_string();
        let route_id = grant.route_id.0.to_string();
        let hostname = grant.hostname.to_ascii_lowercase();
        let token_hmac = grant.token_hmac.to_vec();
        self.call("insert HTTP route grant", move |database| {
            database
                .execute(
                    r"
INSERT INTO route_grants (
    id, browser_session_id, http_route_id, hostname, token_hmac,
    created_at, expires_at
) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
",
                    rusqlite::params![
                        grant.id,
                        session_id,
                        route_id,
                        hostname,
                        token_hmac,
                        grant.created_at.to_string(),
                        grant.expires_at.to_string(),
                    ],
                )
                .map_err(|source| unavailable("insert HTTP route grant", source))?;
            Ok(())
        })
        .await
    }

    /// Verifies a route grant and returns its active parent browser.
    pub(crate) async fn authenticate_route_grant(
        &self,
        grant_id: &str,
        route_id: &HttpRouteId,
        hostname: &str,
        token_hmac: &[u8; 32],
        activity: SessionActivity,
    ) -> Result<Option<AuthenticatedRouteStorageSession>, Report<AuthServiceError>> {
        let grant_id = grant_id.to_owned();
        let route_id = route_id.0.to_string();
        let hostname = hostname.to_ascii_lowercase();
        let token_hmac = token_hmac.to_vec();
        self.call("authenticate HTTP route grant", move |database| {
            let row = database
                .query_row(
                    r"
SELECT g.token_hmac, g.expires_at, s.id, s.origin, s.last_seen_at,
       s.idle_expires_at, s.absolute_expires_at
FROM route_grants AS g
JOIN browser_sessions AS s ON s.id = g.browser_session_id
WHERE g.id = ?1 AND g.http_route_id = ?2 AND g.hostname = ?3
  AND s.revoked_at IS NULL
",
                    rusqlite::params![grant_id, route_id, hostname],
                    |row| {
                        Ok((
                            row.get::<_, Vec<u8>>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                            row.get::<_, String>(4)?,
                            row.get::<_, String>(5)?,
                            row.get::<_, String>(6)?,
                        ))
                    },
                )
                .optional()
                .map_err(|source| unavailable("query HTTP route grant", source))?;
            let Some((
                stored_hmac,
                grant_expires,
                session_id,
                origin,
                last_seen,
                idle_expires,
                absolute_expires,
            )) = row
            else {
                return Ok(None);
            };
            if !bool::from(stored_hmac.ct_eq(token_hmac.as_slice())) {
                return Ok(None);
            }
            let session_id = api::BrowserSessionId::from_str(&session_id)
                .map_err(|source| unavailable("decode route grant browser session", source))?;
            let grant_expires = parse_timestamp(&grant_expires, "expires_at")?;
            let last_seen = parse_timestamp(&last_seen, "last_seen_at")?;
            let idle_expires = parse_timestamp(&idle_expires, "idle_expires_at")?;
            let absolute_expires = parse_timestamp(&absolute_expires, "absolute_expires_at")?;
            let initial_expires = grant_expires.min(idle_expires).min(absolute_expires);
            if initial_expires <= activity.now {
                return Ok(None);
            }
            let touch_after = add_duration(last_seen, activity.touch_interval)?;
            let touched = activity.now >= touch_after;
            let idle_expires = if touched {
                let updated_idle =
                    add_duration(activity.now, activity.idle_lifetime)?.min(absolute_expires);
                database
                    .execute(
                        r"
UPDATE browser_sessions
SET last_seen_at = ?2, idle_expires_at = ?3
WHERE id = ?1 AND revoked_at IS NULL
",
                        rusqlite::params![
                            session_id.0.as_ref(),
                            activity.now.to_string(),
                            updated_idle.to_string(),
                        ],
                    )
                    .map_err(|source| unavailable("touch route browser session", source))?;
                updated_idle
            } else {
                idle_expires
            };
            let expires_at = grant_expires.min(idle_expires).min(absolute_expires);
            if expires_at <= activity.now {
                return Ok(None);
            }
            Ok(Some(AuthenticatedRouteStorageSession {
                session: AuthenticatedSession {
                    id: session_id,
                    origin,
                    expires_at,
                },
                touched,
            }))
        })
        .await
    }

    async fn call<T, F>(
        &self,
        operation: &'static str,
        function: F,
    ) -> Result<T, Report<AuthServiceError>>
    where
        T: Send + 'static,
        F: FnOnce(&mut rusqlite::Connection) -> Result<T, Report<AuthServiceError>>
            + Send
            + 'static,
    {
        self.connection
            .call_raw(function)
            .await
            .map_err(|source| unavailable(operation, source))?
    }
}

fn parse_timestamp(
    value: &str,
    column: &'static str,
) -> Result<Timestamp, Report<AuthServiceError>> {
    Timestamp::from_str(value).map_err(|source| {
        AuthServiceError::Unavailable.report().message(format!(
            "authentication database has invalid {column}: {source}"
        ))
    })
}

fn unavailable(
    operation: &'static str,
    source: impl std::fmt::Display,
) -> Report<AuthServiceError> {
    AuthServiceError::Unavailable
        .report()
        .message(format!("{operation}: {source}"))
}

trait OptionalRow<T> {
    fn optional(self) -> rusqlite::Result<Option<T>>;
}

impl<T> OptionalRow<T> for rusqlite::Result<T> {
    fn optional(self) -> rusqlite::Result<Option<T>> {
        match self {
            Ok(value) => Ok(Some(value)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(error) => Err(error),
        }
    }
}
