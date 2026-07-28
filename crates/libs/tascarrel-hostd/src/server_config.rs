//! Safe loading and validation of host-wide `server.toml` settings.
//!
//! [`ServerConfig`] resolves the optional remote browser origin, its derived
//! HTTP-route hostname suffix, and the authentication key path used at daemon
//! startup.

use std::collections::BTreeSet;
use std::fs::OpenOptions;
use std::io;
use std::io::Read as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Path;
use std::path::PathBuf;

use axum::http::Uri;
use reportify::ErrorExt as _;
use reportify::Report;
use tascarrel_api::types::config as api;
use thiserror::Error;

use crate::NetworkService;
use crate::services::network::validate_hostname_suffix;

/// Validated host-wide settings loaded from `server.toml`.
#[derive(Clone, Debug)]
pub(crate) struct ServerConfig {
    public_origin: Option<String>,
    route_hostname_suffix: String,
    authentication_secret_file: Option<PathBuf>,
}

impl ServerConfig {
    /// Loads `server.toml`, or returns local-only defaults when it is absent.
    ///
    /// # Errors
    ///
    /// Returns an error when the file cannot be read safely, exceeds its size
    /// limit, contains invalid TOML or unknown fields, or specifies an invalid
    /// remote origin.
    #[tracing::instrument(level = "debug", skip_all, fields(path = %path.as_ref().display()), err)]
    pub(crate) fn load(path: impl AsRef<Path>) -> Result<Self, Report<ServerConfigError>> {
        let path = path.as_ref();
        let document = read_document(path)?;
        let (public_origin, route_hostname_suffix) = document
            .remote_access
            .as_ref()
            .map(validate_remote_access)
            .transpose()?
            .map_or_else(
                || (None, NetworkService::LOCAL_HOSTNAME_SUFFIX.to_owned()),
                |(origin, suffix)| (Some(origin), suffix),
            );
        let authentication_secret_file = document
            .authentication
            .and_then(|authentication| authentication.secret_file)
            .map(|secret| resolve_path(path, PathBuf::from(secret.as_ref())))
            .transpose()?;
        Ok(Self {
            public_origin,
            route_hostname_suffix,
            authentication_secret_file,
        })
    }

    /// Returns the canonical externally visible browser origin, when enabled.
    pub(crate) fn public_origin(&self) -> Option<&str> {
        self.public_origin.as_deref()
    }

    /// Returns the DNS suffix used for host-issued HTTP routes.
    pub(crate) fn route_hostname_suffix(&self) -> &str {
        &self.route_hostname_suffix
    }

    /// Returns the configured external authentication key path.
    pub(crate) fn authentication_secret_file(&self) -> Option<&Path> {
        self.authentication_secret_file.as_deref()
    }
}

/// Host-wide configuration loading failures.
#[derive(Debug, Error)]
pub(crate) enum ServerConfigError {
    /// `server.toml` could not be opened.
    #[error("failed to open server.toml")]
    Open,
    /// `server.toml` metadata could not be inspected.
    #[error("failed to inspect server.toml")]
    Inspect,
    /// The configured path is not a regular file.
    #[error("server.toml is not a regular file")]
    NotRegular,
    /// The file exceeds the supported size.
    #[error("server.toml exceeds 64 KiB")]
    TooLarge,
    /// The file contents could not be read.
    #[error("failed to read server.toml")]
    Read,
    /// The file is not UTF-8.
    #[error("server.toml is not UTF-8")]
    NotUtf8,
    /// The document is invalid or contains unsupported fields.
    #[error("failed to decode server.toml")]
    Decode,
    /// The document contains fields outside the Sidex contract.
    #[error("server.toml contains unknown fields: {}", .0.join(", "))]
    UnknownFields(Vec<String>),
    /// The remote browser configuration is unsafe or ambiguous.
    #[error("server.toml contains invalid remote-access settings")]
    InvalidRemoteAccess,
    /// A relative path cannot be resolved beside the configuration file.
    #[error("server.toml contains an invalid authentication key path")]
    InvalidPath,
}

fn read_document(path: &Path) -> Result<api::ServerConfig, Report<ServerConfigError>> {
    let mut file = match OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_CLOEXEC | nix::libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return decode_document("");
        }
        Err(error) => {
            return Err(ServerConfigError::Open.report().message(error.to_string()));
        }
    };
    let metadata = file.metadata().map_err(|error| {
        ServerConfigError::Inspect
            .report()
            .message(error.to_string())
    })?;
    if !metadata.is_file() {
        return Err(ServerConfigError::NotRegular.report());
    }
    if metadata.len() > tascarrel_api::MAX_SERVER_CONFIG_BYTES {
        return Err(ServerConfigError::TooLarge.report());
    }
    let mut bytes = Vec::new();
    file.by_ref()
        .take(tascarrel_api::MAX_SERVER_CONFIG_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| ServerConfigError::Read.report().message(error.to_string()))?;
    if u64::try_from(bytes.len()).map_or(true, |length| {
        length > tascarrel_api::MAX_SERVER_CONFIG_BYTES
    }) {
        return Err(ServerConfigError::TooLarge.report());
    }
    let text = std::str::from_utf8(&bytes).map_err(|_| ServerConfigError::NotUtf8.report())?;
    decode_document(text)
}

/// Decodes the generated TOML contract and rejects unknown fields.
fn decode_document(text: &str) -> Result<api::ServerConfig, Report<ServerConfigError>> {
    let mut unknown = BTreeSet::new();
    let deserializer = toml::Deserializer::parse(text).map_err(|error| {
        ServerConfigError::Decode
            .report()
            .message(error.to_string())
    })?;
    let document = serde_ignored::deserialize(deserializer, |path| {
        unknown.insert(path.to_string());
    })
    .map_err(|error| {
        ServerConfigError::Decode
            .report()
            .message(error.to_string())
    })?;
    if !unknown.is_empty() {
        return Err(ServerConfigError::UnknownFields(unknown.into_iter().collect()).report());
    }
    Ok(document)
}

fn validate_remote_access(
    remote: &api::ServerRemoteAccessConfig,
) -> Result<(String, String), Report<ServerConfigError>> {
    let origin = remote.public_origin.parse::<Uri>().map_err(|error| {
        ServerConfigError::InvalidRemoteAccess
            .report()
            .message(format!("public-origin is not a valid URI: {error}"))
    })?;
    let authority = origin.authority().ok_or_else(|| {
        ServerConfigError::InvalidRemoteAccess
            .report()
            .message("public-origin must include a DNS hostname")
    })?;
    if origin.scheme_str() != Some("https")
        || origin.path() != "/"
        || origin.query().is_some()
        || authority.as_str().contains('@')
    {
        return Err(ServerConfigError::InvalidRemoteAccess.report().message(
            "public-origin must be an HTTPS origin without credentials, a path, query, or fragment",
        ));
    }
    let suffix = authority.host().to_ascii_lowercase();
    validate_hostname_suffix(&suffix).map_err(|error| {
        ServerConfigError::InvalidRemoteAccess
            .report()
            .message(error.to_string())
    })?;
    let port = authority
        .port_u16()
        .filter(|port| *port != 443)
        .map(|port| format!(":{port}"))
        .unwrap_or_default();
    Ok((format!("https://{suffix}{port}"), suffix))
}

fn resolve_path(
    config_path: &Path,
    configured: PathBuf,
) -> Result<PathBuf, Report<ServerConfigError>> {
    if configured.is_absolute() {
        return Ok(configured);
    }
    config_path
        .parent()
        .map(|directory| directory.join(configured))
        .ok_or_else(|| ServerConfigError::InvalidPath.report())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verifies strict host configuration derives the route suffix and
    /// resolves authentication paths without accepting unknown fields.
    #[test]
    fn server_configuration_is_strict_and_derives_remote_settings() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("server.toml");
        std::fs::write(
            &path,
            r#"
[remote-access]
public-origin = "https://Tascarrel.Example.com:443"

[authentication]
secret-file = "auth.key"
"#,
        )
        .unwrap();

        let config = ServerConfig::load(&path).unwrap();
        assert_eq!(
            config.public_origin(),
            Some("https://tascarrel.example.com")
        );
        assert_eq!(config.route_hostname_suffix(), "tascarrel.example.com");
        let expected_secret_file = directory.path().join("auth.key");
        assert_eq!(
            config.authentication_secret_file(),
            Some(expected_secret_file.as_path())
        );

        std::fs::write(&path, "unsupported = true").unwrap();
        assert!(ServerConfig::load(&path).is_err());
    }
}
