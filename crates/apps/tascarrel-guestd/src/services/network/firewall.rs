//! Fail-closed nftables attribution for guest network principals.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::net::Ipv4Addr;
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;

use nix::libc;
use reportify::ErrorExt as _;
use reportify::Report;
use tascarrel_protocol::ErrorCode;
use tascarrel_protocol::RemoteError;
use thiserror::Error;
use tokio::io::AsyncWriteExt as _;
use tokio::process::Command;

const DUMMY_INTERFACE: &str = "tascarrel0";
const DUMMY_ADDRESS: &str = "192.0.2.1/32";
const COMMAND_ERROR_LIMIT: usize = 512;
pub(crate) const POD_NETWORK: &str = "10.64.0.0/10";
pub(crate) const FIRST_PROXY_PORT: u16 = 1;
pub(crate) const LAST_PROXY_PORT: u16 = 1023;
const RESERVED_PROXY_PORTS: &[u16] = &[22, 53, 67, 68, 80, 111, 123, 443, 631];

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NetworkBinding {
    pub(crate) proxy_port: u16,
    origin: NetworkOrigin,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum NetworkOrigin {
    /// Catch-all for guest-local services, including transient Nix build UIDs.
    Guest,
    /// One long-lived guest-local principal with a stable UID.
    System { source_uid: u32 },
    Interface {
        name: String,
        source_address: Ipv4Addr,
        image_build: bool,
    },
}

impl NetworkBinding {
    pub(crate) fn for_veth(
        source_uid: u32,
        proxy_port: u16,
        input_interface: impl Into<String>,
        source_address: Ipv4Addr,
    ) -> Result<Self, RemoteError> {
        if source_uid == 0 {
            return Err(invalid("pod namespace UID must be nonzero"));
        }
        Self::for_interface(proxy_port, input_interface, source_address, false)
    }

    pub(crate) fn for_build_veth(
        proxy_port: u16,
        input_interface: impl Into<String>,
        source_address: Ipv4Addr,
    ) -> Result<Self, RemoteError> {
        Self::for_interface(proxy_port, input_interface, source_address, true)
    }

    fn for_interface(
        proxy_port: u16,
        input_interface: impl Into<String>,
        source_address: Ipv4Addr,
        image_build: bool,
    ) -> Result<Self, RemoteError> {
        validate_proxy_port(proxy_port)?;
        let name = input_interface.into();
        validate_interface_name(&name).map_err(|error| invalid(error.to_string()))?;
        if image_build != name.starts_with("wbb") {
            return Err(invalid(
                "network interface does not match its principal type",
            ));
        }
        if !pod_address_in_tascarrel_network(source_address) {
            return Err(invalid(format!(
                "source address {source_address} is outside {POD_NETWORK}"
            )));
        }
        Ok(Self {
            proxy_port,
            origin: NetworkOrigin::Interface {
                name,
                source_address,
                image_build,
            },
        })
    }

    pub(crate) fn for_system(source_uid: u32, proxy_port: u16) -> Result<Self, RemoteError> {
        validate_proxy_port(proxy_port)?;
        Ok(Self {
            proxy_port,
            origin: NetworkOrigin::System { source_uid },
        })
    }

    pub(crate) fn for_guest(proxy_port: u16) -> Result<Self, RemoteError> {
        validate_proxy_port(proxy_port)?;
        Ok(Self {
            proxy_port,
            origin: NetworkOrigin::Guest,
        })
    }
}

#[derive(Debug, Error)]
pub enum NetworkFirewallError {
    #[error("firewall command path must be absolute: {0}")]
    RelativeCommand(PathBuf),
    #[error("UID {0} occurs more than once in network firewall state")]
    DuplicateUid(u32),
    #[error("proxy port {0} occurs more than once in network firewall state")]
    DuplicateProxyPort(u16),
    #[error("network interface occurs more than once in firewall state: {0}")]
    DuplicateInterface(String),
    #[error("guest-wide network binding occurs more than once in firewall state")]
    DuplicateGuest,
    #[error("invalid network binding: {0}")]
    InvalidBinding(String),
    #[error("failed to start {program}: {source}")]
    Start {
        program: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to write rules to {program}: {source}")]
    WriteRules {
        program: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{program} failed: {detail}")]
    Command { program: PathBuf, detail: String },
}

#[derive(Clone, Debug)]
pub struct NetworkFirewall {
    ip: PathBuf,
    nft: PathBuf,
}

impl NetworkFirewall {
    #[must_use]
    pub fn new(ip: impl Into<PathBuf>, nft: impl Into<PathBuf>) -> Self {
        Self {
            ip: ip.into(),
            nft: nft.into(),
        }
    }

    /// Atomically replaces the guest's Tascarrel routing and rejection rules.
    ///
    /// # Errors
    ///
    /// Returns an error when a binding is invalid or a configured networking
    /// command cannot install the rules.
    #[tracing::instrument(
        name = "tascarrel_guest.network.sync_firewall",
        level = "debug",
        skip(self, active),
        fields(bindings = active.len()),
        err(Debug)
    )]
    pub async fn sync(
        &self,
        active: &[NetworkBinding],
    ) -> Result<(), Report<NetworkFirewallError>> {
        validate_command(&self.ip)?;
        validate_command(&self.nft)?;
        let rules = Self::render(active)?;
        self.ensure_dummy_route().await?;
        run_with_stdin(&self.nft, &["-f", "-"], rules.as_bytes()).await
    }

    /// Renders one complete nftables transaction for the active bindings.
    ///
    /// # Errors
    ///
    /// Returns an error when bindings overlap or contain invalid attribution.
    ///
    /// # Panics
    ///
    /// Panics only if formatting into an in-memory [`String`] fails, which is
    /// an invariant of Rust's string formatter.
    #[allow(clippy::too_many_lines, reason = "a linear ruleset is easier to audit")]
    pub fn render(active: &[NetworkBinding]) -> Result<String, Report<NetworkFirewallError>> {
        let mut mappings = active.to_vec();
        mappings.sort_by_key(|mapping| mapping.proxy_port);
        let mut uids = BTreeSet::new();
        let mut ports = BTreeSet::new();
        let mut interfaces = BTreeSet::new();
        let mut has_guest = false;
        for mapping in &mappings {
            validate_binding(mapping)?;
            if let NetworkOrigin::System { source_uid } = &mapping.origin
                && !uids.insert(*source_uid)
            {
                return Err(NetworkFirewallError::DuplicateUid(*source_uid).report());
            }
            if matches!(&mapping.origin, NetworkOrigin::Guest)
                && std::mem::replace(&mut has_guest, true)
            {
                return Err(NetworkFirewallError::DuplicateGuest.report());
            }
            if !ports.insert(mapping.proxy_port) {
                return Err(NetworkFirewallError::DuplicateProxyPort(mapping.proxy_port).report());
            }
            if let NetworkOrigin::Interface { name, .. } = &mapping.origin
                && !interfaces.insert(name.clone())
            {
                return Err(NetworkFirewallError::DuplicateInterface(name.clone()).report());
            }
        }

        let mut rules = String::from(
            "destroy table inet tascarrel\n\
             table inet tascarrel {\n\
               chain output_nat {\n\
                 type nat hook output priority dstnat; policy accept;\n",
        );
        // Stable-UID services take precedence over the guest-wide fallback so
        // they keep independent concurrency limits and listeners.
        for mapping in &mappings {
            if let NetworkOrigin::System { source_uid } = &mapping.origin {
                writeln!(
                    rules,
                    "    meta skuid {} meta nfproto ipv4 ip daddr != {POD_NETWORK} fib daddr type != local udp dport 53 redirect to :{}",
                    source_uid, mapping.proxy_port,
                )
                .expect("writing a String cannot fail");
                writeln!(
                    rules,
                    "    meta skuid {} meta nfproto ipv4 ip daddr != {POD_NETWORK} fib daddr type != local meta l4proto tcp redirect to :{}",
                    source_uid, mapping.proxy_port,
                )
                .expect("writing a String cannot fail");
            }
        }
        for mapping in &mappings {
            if matches!(&mapping.origin, NetworkOrigin::Guest) {
                writeln!(
                    rules,
                    "    meta nfproto ipv4 ip daddr != {POD_NETWORK} fib daddr type != local udp dport 53 redirect to :{}",
                    mapping.proxy_port
                )
                .expect("writing a String cannot fail");
                writeln!(
                    rules,
                    "    meta nfproto ipv4 ip daddr != {POD_NETWORK} fib daddr type != local meta l4proto tcp redirect to :{}",
                    mapping.proxy_port
                )
                .expect("writing a String cannot fail");
            }
        }
        rules.push_str(
            "  }\n\
               chain prerouting_nat {\n\
                 type nat hook prerouting priority dstnat; policy accept;\n",
        );
        for mapping in &mappings {
            if let NetworkOrigin::Interface {
                name,
                source_address,
                ..
            } = &mapping.origin
            {
                writeln!(
                    rules,
                    "    iifname \"{name}\" ip saddr {source_address} ip daddr != {POD_NETWORK} fib daddr type != local udp dport 53 redirect to :{}",
                    mapping.proxy_port
                )
                .expect("writing a String cannot fail");
                writeln!(
                    rules,
                    "    iifname \"{name}\" ip saddr {source_address} ip daddr != {POD_NETWORK} fib daddr type != local meta l4proto tcp redirect to :{}",
                    mapping.proxy_port
                )
                .expect("writing a String cannot fail");
            }
        }
        rules.push_str(
            "  }\n\
               chain output_filter {\n\
                 type filter hook output priority filter; policy accept;\n",
        );
        for mapping in &mappings {
            match &mapping.origin {
                NetworkOrigin::System { source_uid } => writeln!(
                    rules,
                    "    meta skuid {source_uid} ct status dnat meta nfproto ipv4 ip daddr 127.0.0.1 meta l4proto {{ tcp, udp }} th dport {} accept",
                    mapping.proxy_port
                ),
                NetworkOrigin::Guest => writeln!(
                    rules,
                    "    ct status dnat meta nfproto ipv4 ip daddr 127.0.0.1 meta l4proto {{ tcp, udp }} th dport {} accept",
                    mapping.proxy_port
                ),
                NetworkOrigin::Interface { .. } => continue,
            }
            .expect("writing a String cannot fail");
        }
        rules.push_str(
            "    meta nfproto ipv4 ip daddr != 10.64.0.0/10 fib daddr type != local meta l4proto { tcp, udp } reject with icmpx type admin-prohibited\n\
             meta nfproto ipv6 fib daddr type != local meta l4proto { tcp, udp } reject with icmpx type admin-prohibited\n",
        );
        rules.push_str(
            "  }\n\
               chain input_filter {\n\
                 type filter hook input priority filter; policy accept;\n",
        );
        for mapping in &mappings {
            if let NetworkOrigin::Interface {
                name,
                source_address,
                ..
            } = &mapping.origin
            {
                writeln!(
                    rules,
                    "    iifname \"{name}\" ip saddr {source_address} ct status dnat meta l4proto {{ tcp, udp }} th dport {} accept",
                    mapping.proxy_port
                )
                .expect("writing a String cannot fail");
                writeln!(
                    rules,
                    "    iifname \"{name}\" reject with icmpx type admin-prohibited"
                )
                .expect("writing a String cannot fail");
            }
        }
        rules.push_str(
            "    ip saddr 10.64.0.0/10 reject with icmpx type admin-prohibited\n\
             }\n\
               chain forward_filter {\n\
                 type filter hook forward priority filter; policy accept;\n",
        );
        for interface in interfaces {
            writeln!(
                rules,
                "    iifname \"{interface}\" reject with icmpx type admin-prohibited"
            )
            .expect("writing a String cannot fail");
            writeln!(
                rules,
                "    oifname \"{interface}\" reject with icmpx type admin-prohibited"
            )
            .expect("writing a String cannot fail");
        }
        rules.push_str(
            "    ip saddr 10.64.0.0/10 reject with icmpx type admin-prohibited\n\
               ip daddr 10.64.0.0/10 reject with icmpx type admin-prohibited\n\
               iifname \"wbi*\" reject with icmpx type admin-prohibited\n\
               iifname \"wbb*\" reject with icmpx type admin-prohibited\n\
               oifname \"wbi*\" reject with icmpx type admin-prohibited\n\
               oifname \"wbb*\" reject with icmpx type admin-prohibited\n\
             }\n\
           }\n",
        );
        Ok(rules)
    }

    async fn ensure_dummy_route(&self) -> Result<(), Report<NetworkFirewallError>> {
        let shown = run(&self.ip, &["link", "show", "dev", DUMMY_INTERFACE]).await?;
        if !shown {
            require_success(&self.ip, &["link", "add", DUMMY_INTERFACE, "type", "dummy"]).await?;
        }
        require_success(&self.ip, &["link", "set", "dev", DUMMY_INTERFACE, "up"]).await?;
        require_success(
            &self.ip,
            &[
                "-4",
                "address",
                "replace",
                DUMMY_ADDRESS,
                "dev",
                DUMMY_INTERFACE,
            ],
        )
        .await?;
        require_success(
            &self.ip,
            &["-4", "route", "replace", "default", "dev", DUMMY_INTERFACE],
        )
        .await
    }
}

pub(crate) fn proxy_port_candidates() -> impl Iterator<Item = u16> {
    (FIRST_PROXY_PORT..=LAST_PROXY_PORT).filter(|port| !RESERVED_PROXY_PORTS.contains(port))
}

fn validate_binding(binding: &NetworkBinding) -> Result<(), Report<NetworkFirewallError>> {
    if !is_proxy_port_candidate(binding.proxy_port) {
        return Err(NetworkFirewallError::InvalidBinding(format!(
            "proxy port {} is unavailable",
            binding.proxy_port
        ))
        .report());
    }
    if let NetworkOrigin::Interface {
        name,
        source_address,
        image_build,
    } = &binding.origin
    {
        validate_interface_name(name)?;
        if *image_build != name.starts_with("wbb") {
            return Err(NetworkFirewallError::InvalidBinding(
                "interface principal type mismatch".to_owned(),
            )
            .report());
        }
        if !pod_address_in_tascarrel_network(*source_address) {
            return Err(NetworkFirewallError::InvalidBinding(format!(
                "source address {source_address} is outside {POD_NETWORK}"
            ))
            .report());
        }
    }
    Ok(())
}

fn validate_proxy_port(proxy_port: u16) -> Result<(), RemoteError> {
    if is_proxy_port_candidate(proxy_port) {
        Ok(())
    } else {
        Err(invalid(format!(
            "port {proxy_port} is not a network proxy port"
        )))
    }
}

fn is_proxy_port_candidate(port: u16) -> bool {
    (FIRST_PROXY_PORT..=LAST_PROXY_PORT).contains(&port) && !RESERVED_PROXY_PORTS.contains(&port)
}

fn pod_address_in_tascarrel_network(address: Ipv4Addr) -> bool {
    let address = u32::from(address);
    let start = u32::from(Ipv4Addr::new(10, 64, 0, 0));
    let end = u32::from(Ipv4Addr::new(10, 127, 255, 255));
    (start..=end).contains(&address)
}

fn validate_interface_name(interface: &str) -> Result<(), Report<NetworkFirewallError>> {
    let valid = !interface.is_empty()
        && interface.len() < libc::IFNAMSIZ
        && interface
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'));
    if valid {
        Ok(())
    } else {
        Err(NetworkFirewallError::InvalidBinding(format!(
            "invalid network interface name {interface:?}"
        ))
        .report())
    }
}

fn invalid(message: impl Into<String>) -> RemoteError {
    RemoteError::new(ErrorCode::InvalidRequest, message)
}

fn validate_command(path: &Path) -> Result<(), Report<NetworkFirewallError>> {
    if path.is_absolute() {
        Ok(())
    } else {
        Err(NetworkFirewallError::RelativeCommand(path.to_path_buf()).report())
    }
}

async fn run(program: &Path, args: &[&str]) -> Result<bool, Report<NetworkFirewallError>> {
    let output = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|source| {
            NetworkFirewallError::Start {
                program: program.to_path_buf(),
                source,
            }
            .report()
        })?;
    Ok(output.status.success())
}

async fn require_success(
    program: &Path,
    args: &[&str],
) -> Result<(), Report<NetworkFirewallError>> {
    let output = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|source| {
            NetworkFirewallError::Start {
                program: program.to_path_buf(),
                source,
            }
            .report()
        })?;
    if output.status.success() {
        Ok(())
    } else {
        Err(command_error(program, &output.stderr).report())
    }
}

async fn run_with_stdin(
    program: &Path,
    args: &[&str],
    input: &[u8],
) -> Result<(), Report<NetworkFirewallError>> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|source| {
            NetworkFirewallError::Start {
                program: program.to_path_buf(),
                source,
            }
            .report()
        })?;
    let mut stdin = child.stdin.take().ok_or_else(|| {
        NetworkFirewallError::Command {
            program: program.to_path_buf(),
            detail: "command did not expose stdin".to_owned(),
        }
        .report()
    })?;
    stdin.write_all(input).await.map_err(|source| {
        NetworkFirewallError::WriteRules {
            program: program.to_path_buf(),
            source,
        }
        .report()
    })?;
    drop(stdin);
    let output = child.wait_with_output().await.map_err(|source| {
        NetworkFirewallError::Start {
            program: program.to_path_buf(),
            source,
        }
        .report()
    })?;
    if output.status.success() {
        Ok(())
    } else {
        Err(command_error(program, &output.stderr).report())
    }
}

fn command_error(program: &Path, stderr: &[u8]) -> NetworkFirewallError {
    let mut detail = String::from_utf8_lossy(stderr).trim().to_owned();
    detail.truncate(detail.floor_char_boundary(COMMAND_ERROR_LIMIT));
    NetworkFirewallError::Command {
        program: program.to_path_buf(),
        detail,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verifies pod rules redirect only DNS UDP and reject other traffic from
    /// the authenticated veth.
    #[test]
    fn rules_redirect_only_dns_udp_and_reject_remaining_udp() {
        let binding =
            NetworkBinding::for_veth(1_000_000, 1001, "wbi00000001", Ipv4Addr::new(10, 64, 0, 2))
                .unwrap();
        let rules = NetworkFirewall::render(&[binding]).unwrap();
        assert!(rules.contains("udp dport 53 redirect to :1001"));
        assert!(!rules.contains("meta l4proto udp redirect to :1001"));
        assert!(rules.contains("iifname \"wbi00000001\" reject"));
    }

    /// Verifies stable guest services are attributed by their effective UID.
    #[test]
    fn system_rules_are_uid_attributed() {
        let binding = NetworkBinding::for_system(777, 1001).unwrap();
        let rules = NetworkFirewall::render(&[binding]).unwrap();
        assert!(rules.contains("meta skuid 777"));
        assert!(rules.contains("udp dport 53 redirect to :1001"));
        assert!(rules.contains("meta l4proto tcp redirect to :1001"));
    }

    /// Verifies the guest fallback captures transient build UIDs after exact
    /// UID rules and rejects unsupported external transports.
    #[test]
    fn guest_rules_capture_transient_build_uids_and_reject_other_udp() {
        let guest = NetworkBinding::for_guest(1001).unwrap();
        let harness = NetworkBinding::for_system(777, 1002).unwrap();
        let rules = NetworkFirewall::render(&[guest, harness]).unwrap();
        let exact = rules.find("meta skuid 777 meta nfproto ipv4").unwrap();
        let fallback = rules
            .find("meta nfproto ipv4 ip daddr != 10.64.0.0/10 fib daddr type != local udp dport 53 redirect to :1001")
            .unwrap();
        assert!(exact < fallback);
        assert!(rules.contains(
            "meta nfproto ipv4 ip daddr != 10.64.0.0/10 fib daddr type != local meta l4proto { tcp, udp } reject"
        ));
        assert!(rules.contains(
            "meta nfproto ipv6 fib daddr type != local meta l4proto { tcp, udp } reject"
        ));
    }

    /// Verifies the input and forwarding base chains are separate nftables
    /// declarations rather than accidentally nested chains.
    #[test]
    fn input_chain_closes_before_forward_chain() {
        let rules = NetworkFirewall::render(&[]).unwrap();
        assert!(rules.contains(
            "ip saddr 10.64.0.0/10 reject with icmpx type admin-prohibited\n}\nchain forward_filter {"
        ));
    }
}
