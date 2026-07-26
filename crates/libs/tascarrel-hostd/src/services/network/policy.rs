//! Workspace network admission and HTTP secret-injection policy.

use std::collections::BTreeSet;
use std::net::IpAddr;
use std::path::Path;

use reportify::ErrorExt as _;
use reportify::Report;
use tascarrel_api::types::config as config_api;
use thiserror::Error;

use crate::services::config::DEFAULT_MAX_CONFIG_BYTES;
use crate::services::config::load_config_file;
use crate::services::secrets::SecretReference;

pub(crate) const MAX_SECRET_BYTES: usize = 64 * 1024;
const MAX_SECRET_RULES: usize = 64;
const MAX_HOST_PORTS: usize = 64;
const MAX_POLICY_PORTS: usize = 256;

#[derive(Debug, Error)]
pub(crate) enum NetworkPolicyError {
    #[error("workspace network policy is invalid: {0}")]
    Invalid(String),
    #[error("failed to inspect host network interfaces: {0}")]
    HostInterfaces(String),
}

#[derive(Clone, Debug)]
pub struct NetworkPolicy {
    pub host_ports: Vec<HostPortMapping>,
    pub default_deny: bool,
    pub allow_local: bool,
    pub allow_addresses: Vec<IpAddr>,
    pub deny_addresses: Vec<IpAddr>,
    pub allow_hosts: Vec<String>,
    pub deny_hosts: Vec<String>,
    pub allow_ports: Vec<u16>,
    pub secrets: Option<config_api::WorkspaceSecretsConfig>,
    pub secret_injection: Vec<SecretInjection>,
    pub http_ports: Vec<u16>,
    pub https_ports: Vec<u16>,
}

/// One static host-loopback port exposed at a pod-visible virtual port.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct HostPortMapping {
    pub(crate) host_port: u16,
    pub(crate) pod_port: u16,
}

#[derive(Clone)]
pub struct SecretInjection {
    pub host: String,
    pub header: Option<String>,
    pub placeholder: String,
    pub reference: SecretReference,
}

impl std::fmt::Debug for SecretInjection {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SecretInjection")
            .field("host", &self.host)
            .field("header", &self.header)
            .field("placeholder", &self.placeholder)
            .field("reference", &"[SECRET REFERENCE]")
            .finish()
    }
}

impl NetworkPolicy {
    /// Returns a fail-closed policy used while workspace configuration is
    /// invalid.
    #[must_use]
    pub fn deny_all() -> Self {
        Self {
            default_deny: true,
            allow_ports: Vec::new(),
            ..Self::default()
        }
    }

    /// Loads and validates host-side network policy and secret references.
    ///
    /// # Errors
    ///
    /// Returns an error for an unsafe configuration, invalid rule, or missing
    /// secret.
    #[allow(
        clippy::too_many_lines,
        reason = "policy loading validates one cohesive workspace network document"
    )]
    pub fn load(path: &Path) -> Result<Self, Report<NetworkPolicyError>> {
        let parsed = load_config_file(path, DEFAULT_MAX_CONFIG_BYTES)
            .map_err(|error| invalid_policy(error.to_string()))?;
        let secret_providers = parsed.secrets.clone();
        let Some(network) = parsed.network else {
            return Ok(Self::default());
        };
        let host_ports = network
            .host_ports
            .unwrap_or_default()
            .iter()
            .map(host_port_mapping)
            .collect::<Result<Vec<_>, _>>()?;
        let allow_ports = network
            .allow_ports
            .map_or_else(default_allow_ports, |ports| ports.into_iter().collect());
        let plain_http_ports = network
            .http_ports
            .unwrap_or_default()
            .into_iter()
            .collect::<Vec<_>>();
        let tls_http_ports = network
            .https_ports
            .unwrap_or_default()
            .into_iter()
            .collect::<Vec<_>>();
        let allow_addresses = parse_addresses(network.allow_addresses)?;
        let deny_addresses = parse_addresses(network.deny_addresses)?;
        let allow_hosts = network
            .allow_hosts
            .unwrap_or_default()
            .into_iter()
            .map(Into::into)
            .collect::<Vec<String>>();
        let deny_hosts = network
            .deny_hosts
            .unwrap_or_default()
            .into_iter()
            .map(Into::into)
            .collect::<Vec<String>>();
        let rules = network.secret_injection.unwrap_or_default();
        if rules.len() > MAX_SECRET_RULES {
            return Err(invalid_policy("too many HTTP secret-injection rules"));
        }
        validate_ports(&allow_ports, "allow-ports")?;
        validate_ports(&plain_http_ports, "http-ports")?;
        validate_ports(&tls_http_ports, "https-ports")?;
        if host_ports.len() > MAX_HOST_PORTS {
            return Err(invalid_policy(format!(
                "network host-ports is limited to {MAX_HOST_PORTS} entries"
            )));
        }
        let distinct_pod_ports = host_ports
            .iter()
            .map(|mapping| mapping.pod_port)
            .collect::<BTreeSet<_>>();
        if distinct_pod_ports.len() != host_ports.len() {
            return Err(invalid_policy(
                "network host-port pod-side ports must be unique",
            ));
        }
        let mut secret_injection = Vec::with_capacity(rules.len());
        for rule in rules {
            validate_host_pattern(&rule.host)?;
            let reference = SecretReference::parse(rule.secret.as_ref())
                .map_err(|error| invalid_policy(error.to_string()))?;
            let placeholder = rule
                .placeholder
                .map_or_else(|| infer_placeholder(reference.secret()), Into::into);
            if rule
                .header
                .as_deref()
                .is_some_and(|header| !valid_header_name(header) || forbidden_secret_header(header))
                || placeholder.is_empty()
                || placeholder.len() > MAX_SECRET_BYTES
                || placeholder.contains(['\r', '\n'])
            {
                return Err(invalid_policy("invalid HTTP secret-injection rule"));
            }
            let configured = secret_providers
                .as_ref()
                .and_then(|secrets| secrets.providers.as_ref())
                .is_some_and(|providers| providers.contains_key(reference.provider()));
            if !configured {
                return Err(invalid_policy(
                    "HTTP secret-injection rule references an unconfigured provider",
                ));
            }
            secret_injection.push(SecretInjection {
                host: rule.host.to_ascii_lowercase(),
                header: rule.header.map(|header| header.to_ascii_lowercase()),
                placeholder,
                reference,
            });
        }
        for host in allow_hosts.iter().chain(&deny_hosts) {
            validate_host_pattern(host)?;
        }
        let default_deny = match network.default.as_deref().unwrap_or("allow") {
            "allow" => false,
            "deny" => true,
            value => {
                return Err(invalid_policy(format!(
                    "invalid network default action {value:?}"
                )));
            }
        };
        Ok(Self {
            host_ports,
            default_deny,
            allow_local: network.allow_local.unwrap_or(false),
            allow_addresses,
            deny_addresses,
            allow_hosts: lower(allow_hosts),
            deny_hosts: lower(deny_hosts),
            allow_ports,
            secrets: secret_providers,
            secret_injection,
            http_ports: if plain_http_ports.is_empty() {
                vec![80]
            } else {
                plain_http_ports
            },
            https_ports: if tls_http_ports.is_empty() {
                vec![443]
            } else {
                tls_http_ports
            },
        })
    }

    /// Returns whether HTTP Host or TLS SNI must be inspected.
    #[must_use]
    pub fn needs_hostname_inspection(&self) -> bool {
        !self.secret_injection.is_empty()
            || !self.allow_hosts.is_empty()
            || !self.deny_hosts.is_empty()
    }

    /// Returns whether HTTPS secret injection needs a workspace CA.
    #[must_use]
    pub fn requires_https_authority(&self) -> bool {
        !self.secret_injection.is_empty()
    }

    /// Returns whether HTTPS requests for a host require interception.
    #[must_use]
    pub(crate) fn injects_secret_for_host(&self, host: &str) -> bool {
        self.secret_injection
            .iter()
            .any(|rule| host_matches(&rule.host, host))
    }

    #[must_use]
    pub fn host_allowed(&self, host: &str) -> bool {
        let host = host.to_ascii_lowercase();
        if self.deny_hosts.iter().any(|rule| host_matches(rule, &host)) {
            return false;
        }
        self.allow_hosts
            .iter()
            .any(|rule| host_matches(rule, &host))
            || !self.default_deny
    }

    pub(crate) fn rule_matches(pattern: &str, host: &str) -> bool {
        host_matches(pattern, host)
    }

    pub(crate) fn address_allowed(
        &self,
        address: IpAddr,
    ) -> Result<bool, Report<NetworkPolicyError>> {
        if self.deny_addresses.contains(&address) {
            return Ok(false);
        }
        let host_addresses = host_interface_addresses()?;
        Ok(!forbidden_address(
            address,
            self.allow_local,
            &host_addresses,
        ))
    }
}

pub(crate) fn forbidden_address(
    address: IpAddr,
    allow_local: bool,
    host_addresses: &BTreeSet<IpAddr>,
) -> bool {
    if allow_local {
        return false;
    }
    match address {
        IpAddr::V4(address) => {
            address.is_loopback()
                || address.is_private()
                || address.is_link_local()
                || address.is_broadcast()
                || address.is_documentation()
                || address.is_unspecified()
                || address.is_multicast()
                || host_addresses.contains(&IpAddr::V4(address))
        }
        IpAddr::V6(address) => {
            address.is_loopback()
                || address.is_unspecified()
                || address.is_multicast()
                || address.is_unique_local()
                || address.is_unicast_link_local()
                || host_addresses.contains(&IpAddr::V6(address))
        }
    }
}

pub(crate) fn host_interface_addresses() -> Result<BTreeSet<IpAddr>, Report<NetworkPolicyError>> {
    nix::ifaddrs::getifaddrs()
        .map_err(|error| NetworkPolicyError::HostInterfaces(error.to_string()).report())
        .map(|interfaces| {
            interfaces
                .filter_map(|interface| {
                    interface.address.and_then(|address| {
                        address
                            .as_sockaddr_in()
                            .map(|address| IpAddr::V4(address.ip()))
                            .or_else(|| {
                                address
                                    .as_sockaddr_in6()
                                    .map(|address| IpAddr::V6(address.ip()))
                            })
                    })
                })
                .collect()
        })
}

impl Default for NetworkPolicy {
    fn default() -> Self {
        Self {
            host_ports: Vec::new(),
            default_deny: false,
            allow_local: false,
            allow_addresses: Vec::new(),
            deny_addresses: Vec::new(),
            allow_hosts: Vec::new(),
            deny_hosts: Vec::new(),
            allow_ports: default_allow_ports(),
            secrets: None,
            secret_injection: Vec::new(),
            http_ports: vec![80],
            https_ports: vec![443],
        }
    }
}

fn default_allow_ports() -> Vec<u16> {
    vec![80, 443]
}

/// Converts one generated config value into an enforced host-port mapping.
fn host_port_mapping(
    mapping: &config_api::WorkspaceHostPort,
) -> Result<HostPortMapping, Report<NetworkPolicyError>> {
    let (host_port, pod_port) = match mapping {
        config_api::WorkspaceHostPort::SamePort(port) if *port != 0 => (*port, *port),
        config_api::WorkspaceHostPort::SamePort(_) => {
            return Err(invalid_policy(
                "network host-port shorthand must be nonzero",
            ));
        }
        config_api::WorkspaceHostPort::Mapping(mapping) => mapping
            .ports()
            .map_err(|error| invalid_policy(error.to_string()))?,
    };
    Ok(HostPortMapping {
        host_port,
        pod_port,
    })
}

fn parse_addresses(
    values: Option<tascarrel_api::ArcVec<tascarrel_api::ArcStr>>,
) -> Result<Vec<IpAddr>, Report<NetworkPolicyError>> {
    values
        .unwrap_or_default()
        .into_iter()
        .map(|value| {
            value
                .parse()
                .map_err(|_| invalid_policy(format!("invalid network address {value:?}")))
        })
        .collect()
}

fn infer_placeholder(secret_name: &str) -> String {
    format!(
        "tascarrel-secret:{}",
        secret_name.to_ascii_lowercase().replace('_', "-")
    )
}

fn validate_ports(ports: &[u16], name: &str) -> Result<(), Report<NetworkPolicyError>> {
    if ports.len() > MAX_POLICY_PORTS || ports.contains(&0) {
        return Err(invalid_policy(format!("invalid network {name}")));
    }
    let mut sorted = ports.to_vec();
    sorted.sort_unstable();
    if sorted.windows(2).any(|ports| ports[0] == ports[1]) {
        return Err(invalid_policy(format!("duplicate port in network {name}")));
    }
    Ok(())
}

fn lower(values: Vec<String>) -> Vec<String> {
    values
        .into_iter()
        .map(|value| value.to_ascii_lowercase())
        .collect()
}

fn host_matches(pattern: &str, host: &str) -> bool {
    pattern
        .strip_prefix("*.")
        .map_or(pattern == host, |suffix| {
            host.len() > suffix.len()
                && host.ends_with(suffix)
                && host.as_bytes()[host.len() - suffix.len() - 1] == b'.'
        })
}

fn validate_host_pattern(host: &str) -> Result<(), Report<NetworkPolicyError>> {
    let labels = host.strip_prefix("*.").unwrap_or(host);
    if labels.is_empty()
        || labels.len() > 253
        || labels.split('.').any(|label| {
            label.is_empty()
                || !label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
    {
        return Err(invalid_policy(format!(
            "invalid network host pattern {host:?}"
        )));
    }
    Ok(())
}

fn invalid_policy(message: impl Into<String>) -> Report<NetworkPolicyError> {
    NetworkPolicyError::Invalid(message.into()).report()
}

fn valid_header_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte))
}

pub(crate) fn forbidden_secret_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "connection"
            | "content-length"
            | "host"
            | "keep-alive"
            | "proxy-connection"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    )
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    /// Verifies exact, wildcard, and deny host rules use normalized subdomain
    /// matching.
    #[test]
    fn host_rules_support_exact_and_subdomain_patterns() {
        let policy = NetworkPolicy {
            default_deny: true,
            allow_hosts: vec!["api.example".into(), "*.trusted.example".into()],
            deny_hosts: vec!["blocked.trusted.example".into()],
            ..NetworkPolicy::default()
        };
        assert!(policy.host_allowed("API.EXAMPLE"));
        assert!(policy.host_allowed("one.trusted.example"));
        assert!(!policy.host_allowed("trusted.example"));
        assert!(!policy.host_allowed("blocked.trusted.example"));
        assert!(!policy.host_allowed("untrusted.example"));
    }

    /// Verifies an absent network section produces the safe default port
    /// policy.
    #[test]
    fn config_without_network_policy_allows_only_web_ports_by_default() {
        let directory = tempfile::tempdir().unwrap();
        let config = directory.path().join("config.toml");
        fs::write(&config, "[env]\nEDITOR = 'vim'\n").unwrap();
        let policy = NetworkPolicy::load(&config).unwrap();
        assert!(policy.host_ports.is_empty());
        assert!(!policy.default_deny);
        assert!(!policy.allow_local);
        assert_eq!(policy.allow_ports, [80, 443]);
        assert_eq!(policy.http_ports, [80]);
        assert_eq!(policy.https_ports, [443]);
    }

    /// Verifies explicit ports replace defaults and secret placeholders remain
    /// optional while legacy keys are rejected.
    #[test]
    fn config_can_replace_allowed_ports_and_omit_injection_header() {
        let directory = tempfile::tempdir().unwrap();
        let config = directory.path().join("config.toml");
        fs::write(
            &config,
            "[secrets.providers.project]\nkind = 'sops'\n\
             [network]\nhost-ports = [3000, '5432:15432']\nallow-ports = [22, 8443]\n\
             [[network.secret-injection]]\nhost = 'api.example'\n\
             secret = 'project.API_TOKEN'\n",
        )
        .unwrap();
        let parsed = load_config_file(&config, DEFAULT_MAX_CONFIG_BYTES).unwrap();
        let network = parsed.network.unwrap();
        assert_eq!(network.host_ports.as_ref().unwrap().len(), 2);
        assert_eq!(network.allow_ports.unwrap().as_ref(), [22, 8443]);
        let secret_injection = network.secret_injection.unwrap();
        assert!(secret_injection[0].header.is_none());
        assert!(secret_injection[0].placeholder.is_none());
        assert_eq!(infer_placeholder("API_TOKEN"), "tascarrel-secret:api-token");

        fs::write(&config, "[network]\nhost-ports = [3000, '5432:15432']\n").unwrap();
        let policy = NetworkPolicy::load(&config).unwrap();
        assert_eq!(
            policy.host_ports,
            [
                HostPortMapping {
                    host_port: 3000,
                    pod_port: 3000,
                },
                HostPortMapping {
                    host_port: 5432,
                    pod_port: 15432,
                },
            ]
        );
        fs::write(
            &config,
            "[network]\n[[network.secret-injection]]\nhost = 'api.example'\n\
             placeholder = 'x'\nsecret-env = 'TASCARREL_TOKEN'\n",
        )
        .unwrap();
        assert!(load_config_file(&config, DEFAULT_MAX_CONFIG_BYTES).is_err());
    }
}
