//! Network namespace and veth management for guest workloads.

use std::net::Ipv4Addr;
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use thiserror::Error;
use tokio::process::Command;
use tokio::time::timeout;

use crate::runtime::command::spawn;

const POD_NETWORK_BASE: u32 = u32::from_be_bytes([10, 64, 0, 0]);
const POD_NETWORK_COUNT: u32 = 1 << 20;
const BUILD_NETWORK_SLOT: u32 = POD_NETWORK_COUNT - 1;
/// Fixed named network namespace used only while resolving the workspace image.
pub const BUILD_NETWORK_NAMESPACE: &str = "tascarrel-build";
const COMMAND_ERROR_LIMIT: usize = 512;
const DEFAULT_COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

/// Trusted addresses and interface names allocated to one pod namespace.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PodNetwork {
    pub host_interface: String,
    pub host_address: Ipv4Addr,
    pub pod_address: Ipv4Addr,
}

impl PodNetwork {
    /// Deterministically allocates a /30 from Tascarrel's private 10.64.0.0/10
    /// range.
    ///
    /// # Errors
    ///
    /// Returns an error when the durable pod slot exceeds the address pool.
    pub fn for_slot(slot: u32) -> Result<Self, NetworkError> {
        // Keep the final /30 out of the durable pod allocator. It gives image
        // builds a stable kernel-authenticated veth identity without sharing a
        // pod address or relying on the UID selected by a Dockerfile step.
        if slot >= BUILD_NETWORK_SLOT {
            return Err(NetworkError::SlotExhausted(slot));
        }
        let subnet = POD_NETWORK_BASE + slot * 4;
        Ok(Self {
            host_interface: format!("wbi{slot:08x}"),
            host_address: Ipv4Addr::from(subnet + 1),
            pod_address: Ipv4Addr::from(subnet + 2),
        })
    }

    /// Allocates the /30 reserved for the short-lived workspace image build.
    #[must_use]
    pub fn for_build() -> Self {
        let subnet = POD_NETWORK_BASE + BUILD_NETWORK_SLOT * 4;
        Self {
            host_interface: "wbb00000000".into(),
            host_address: Ipv4Addr::from(subnet + 1),
            pod_address: Ipv4Addr::from(subnet + 2),
        }
    }

    fn peer_interface(&self) -> String {
        if self.host_interface.starts_with("wbb") {
            self.host_interface.replacen("wbb", "wpb", 1)
        } else {
            self.host_interface.replacen("wbi", "wpi", 1)
        }
    }
}

/// Creates and removes pod veths while runc owns namespace creation.
#[derive(Clone, Debug)]
pub struct NetworkManager {
    ip: PathBuf,
    nsenter: PathBuf,
    command_timeout: Duration,
}

impl NetworkManager {
    /// Creates a network manager using absolute `ip` and `nsenter` paths.
    ///
    /// # Errors
    ///
    /// Returns an error when either command path is relative.
    pub fn new(ip: impl Into<PathBuf>, nsenter: impl Into<PathBuf>) -> Result<Self, NetworkError> {
        let ip = ip.into();
        let nsenter = nsenter.into();
        for path in [&ip, &nsenter] {
            if !path.is_absolute() {
                return Err(NetworkError::RelativeCommand(path.clone()));
            }
        }
        Ok(Self {
            ip,
            nsenter,
            command_timeout: DEFAULT_COMMAND_TIMEOUT,
        })
    }

    /// Connects the guest namespace to a paused runc container's network
    /// namespace.
    ///
    /// # Errors
    ///
    /// Returns an error for PID zero or when a veth or namespace command fails.
    pub async fn create(&self, network: &PodNetwork, pid: u32) -> Result<(), NetworkError> {
        if pid == 0 {
            return Err(NetworkError::InvalidPid);
        }
        let peer = network.peer_interface();
        self.require(
            &self.ip,
            &[
                "link",
                "add",
                &network.host_interface,
                "type",
                "veth",
                "peer",
                "name",
                &peer,
            ],
        )
        .await?;
        let result = async {
            self.require(&self.ip, &["link", "set", &peer, "netns", &pid.to_string()])
                .await?;
            self.require(
                &self.ip,
                &[
                    "address",
                    "replace",
                    &format!("{}/30", network.host_address),
                    "dev",
                    &network.host_interface,
                ],
            )
            .await?;
            self.require(
                &self.ip,
                &["link", "set", "dev", &network.host_interface, "up"],
            )
            .await?;
            self.in_namespace(pid, &["link", "set", "dev", "lo", "up"])
                .await?;
            self.in_namespace(pid, &["link", "set", "dev", &peer, "name", "eth0"])
                .await?;
            self.in_namespace(
                pid,
                &[
                    "address",
                    "replace",
                    &format!("{}/30", network.pod_address),
                    "dev",
                    "eth0",
                ],
            )
            .await?;
            self.in_namespace(pid, &["link", "set", "dev", "eth0", "up"])
                .await?;
            self.in_namespace(
                pid,
                &[
                    "route",
                    "replace",
                    "default",
                    "via",
                    &network.host_address.to_string(),
                    "dev",
                    "eth0",
                ],
            )
            .await
        }
        .await;
        if result.is_err() {
            let _ = self.remove(network).await;
        }
        result
    }

    /// Creates and configures a named namespace for an image build.
    ///
    /// `BuildKit` is launched in this namespace with host networking relative
    /// to the namespace. Consequently image pulls and every `RUN`,
    /// including commands after Dockerfile `USER`, enter the guest egress
    /// service through one trusted veth.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid namespace name or failed `ip` command.
    pub async fn create_named(
        &self,
        network: &PodNetwork,
        namespace: &str,
    ) -> Result<(), NetworkError> {
        validate_namespace_name(namespace)?;
        // A service restart can leave the named namespace mounted even after
        // systemd has killed BuildKit. Remove both ends before attempting to
        // recreate the fixed identity.
        self.remove_named(network, namespace).await?;
        self.require(&self.ip, &["netns", "add", namespace]).await?;
        let peer = network.peer_interface();
        let result = async {
            self.require(
                &self.ip,
                &[
                    "link",
                    "add",
                    &network.host_interface,
                    "type",
                    "veth",
                    "peer",
                    "name",
                    &peer,
                ],
            )
            .await?;
            self.require(&self.ip, &["link", "set", &peer, "netns", namespace])
                .await?;
            self.require(
                &self.ip,
                &[
                    "address",
                    "replace",
                    &format!("{}/30", network.host_address),
                    "dev",
                    &network.host_interface,
                ],
            )
            .await?;
            self.require(
                &self.ip,
                &["link", "set", "dev", &network.host_interface, "up"],
            )
            .await?;
            self.in_named_namespace(namespace, &["link", "set", "dev", "lo", "up"])
                .await?;
            self.in_named_namespace(namespace, &["link", "set", "dev", &peer, "name", "eth0"])
                .await?;
            self.in_named_namespace(
                namespace,
                &[
                    "address",
                    "replace",
                    &format!("{}/30", network.pod_address),
                    "dev",
                    "eth0",
                ],
            )
            .await?;
            self.in_named_namespace(namespace, &["link", "set", "dev", "eth0", "up"])
                .await?;
            self.in_named_namespace(
                namespace,
                &[
                    "route",
                    "replace",
                    "default",
                    "via",
                    &network.host_address.to_string(),
                    "dev",
                    "eth0",
                ],
            )
            .await
        }
        .await;
        if result.is_err() {
            let _ = self.remove_named(network, namespace).await;
        }
        result
    }

    /// Deletes the host end; Linux removes the peer in the pod namespace.
    ///
    /// # Errors
    ///
    /// Returns an error when the link cannot be inspected or removed.
    pub async fn remove(&self, network: &PodNetwork) -> Result<(), NetworkError> {
        let output = self
            .run(
                &self.ip,
                &["link", "delete", "dev", &network.host_interface],
            )
            .await?;
        if output.status.success() || !self.link_exists(&network.host_interface).await? {
            Ok(())
        } else {
            Err(command_failed(&self.ip, &output.stderr))
        }
    }

    /// Removes a named image-build namespace and its veth. Missing state is
    /// accepted so guest daemon restart can discard an interrupted build.
    ///
    /// # Errors
    ///
    /// Returns an error when an existing link or namespace cannot be removed.
    pub async fn remove_named(
        &self,
        network: &PodNetwork,
        namespace: &str,
    ) -> Result<(), NetworkError> {
        validate_namespace_name(namespace)?;
        self.remove(network).await?;
        let output = self.run(&self.ip, &["netns", "delete", namespace]).await?;
        if output.status.success() || !namespace_path(namespace).exists() {
            Ok(())
        } else {
            Err(command_failed(&self.ip, &output.stderr))
        }
    }

    async fn link_exists(&self, interface: &str) -> Result<bool, NetworkError> {
        Ok(self
            .run(&self.ip, &["link", "show", "dev", interface])
            .await?
            .status
            .success())
    }

    async fn in_namespace(&self, pid: u32, arguments: &[&str]) -> Result<(), NetworkError> {
        let pid = pid.to_string();
        let ip = self
            .ip
            .to_str()
            .ok_or_else(|| NetworkError::NonUtf8Command(self.ip.clone()))?;
        let mut command = vec!["--target", &pid, "--net", "--", ip];
        command.extend_from_slice(arguments);
        self.require(&self.nsenter, &command).await
    }

    async fn in_named_namespace(
        &self,
        namespace: &str,
        arguments: &[&str],
    ) -> Result<(), NetworkError> {
        let mut command = vec!["-n", namespace];
        command.extend_from_slice(arguments);
        self.require(&self.ip, &command).await
    }

    async fn require(&self, program: &Path, arguments: &[&str]) -> Result<(), NetworkError> {
        let output = self.run(program, arguments).await?;
        if output.status.success() {
            Ok(())
        } else {
            Err(command_failed(program, &output.stderr))
        }
    }

    async fn run(
        &self,
        program: &Path,
        arguments: &[&str],
    ) -> Result<std::process::Output, NetworkError> {
        let mut command = Command::new(program);
        command
            .args(arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let child = spawn(&mut command)
            .await
            .map_err(|source| NetworkError::Start {
                program: program.to_path_buf(),
                source,
            })?;
        timeout(self.command_timeout, child.wait_with_output())
            .await
            .map_err(|_| NetworkError::TimedOut {
                program: program.to_path_buf(),
                timeout: self.command_timeout,
            })?
            .map_err(|source| NetworkError::Start {
                program: program.to_path_buf(),
                source,
            })
    }
}

#[derive(Debug, Error)]
pub enum NetworkError {
    #[error("network command must be absolute: {0}")]
    RelativeCommand(PathBuf),
    #[error("network command path is not UTF-8: {0}")]
    NonUtf8Command(PathBuf),
    #[error("pod network slot {0} is exhausted")]
    SlotExhausted(u32),
    #[error("container PID must be nonzero")]
    InvalidPid,
    #[error("invalid network namespace name: {0}")]
    InvalidNamespace(String),
    #[error("could not start {program}: {source}")]
    Start {
        program: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{program} failed: {detail}")]
    Command { program: PathBuf, detail: String },
    #[error(
        "network command {program} exceeded {} seconds",
        timeout.as_secs()
    )]
    TimedOut { program: PathBuf, timeout: Duration },
}

fn validate_namespace_name(namespace: &str) -> Result<(), NetworkError> {
    if namespace.is_empty()
        || namespace.len() > 63
        || !namespace
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        Err(NetworkError::InvalidNamespace(namespace.into()))
    } else {
        Ok(())
    }
}

fn namespace_path(namespace: &str) -> PathBuf {
    Path::new("/run/netns").join(namespace)
}

fn command_failed(program: &Path, stderr: &[u8]) -> NetworkError {
    let detail = String::from_utf8_lossy(&stderr[..stderr.len().min(COMMAND_ERROR_LIMIT)])
        .trim()
        .to_owned();
    NetworkError::Command {
        program: program.to_path_buf(),
        detail: if detail.is_empty() {
            "command exited unsuccessfully".into()
        } else {
            detail
        },
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use tempfile::tempdir;

    use super::*;

    fn command_in_path(name: &str) -> PathBuf {
        std::env::var_os("PATH")
            .into_iter()
            .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
            .map(|directory| directory.join(name))
            .find(|path| path.is_file())
            .unwrap_or_else(|| panic!("{name} was not found in PATH"))
    }

    #[test]
    fn allocations_are_disjoint_valid_interface_names() {
        let first = PodNetwork::for_slot(0).unwrap();
        let second = PodNetwork::for_slot(1).unwrap();
        assert_eq!(first.host_address, Ipv4Addr::new(10, 64, 0, 1));
        assert_eq!(first.pod_address, Ipv4Addr::new(10, 64, 0, 2));
        assert_eq!(second.host_address, Ipv4Addr::new(10, 64, 0, 5));
        assert_eq!(second.pod_address, Ipv4Addr::new(10, 64, 0, 6));
        assert!(first.host_interface.len() <= 15);
        assert_ne!(first.host_interface, second.host_interface);
        let build = PodNetwork::for_build();
        assert_eq!(build.host_address, Ipv4Addr::new(10, 127, 255, 253));
        assert_eq!(build.pod_address, Ipv4Addr::new(10, 127, 255, 254));
        assert_eq!(build.host_interface, "wbb00000000");
        assert_eq!(build.peer_interface(), "wpb00000000");
        assert!(PodNetwork::for_slot(BUILD_NETWORK_SLOT).is_err());
        assert_ne!(first.host_address, build.host_address);
    }

    #[test]
    fn named_namespace_validation_is_strict() {
        assert!(validate_namespace_name(BUILD_NETWORK_NAMESPACE).is_ok());
        assert!(validate_namespace_name("").is_err());
        assert!(validate_namespace_name("../host").is_err());
        assert!(validate_namespace_name("contains space").is_err());
    }

    #[tokio::test]
    async fn named_creation_removes_stale_identity_before_recreating_it() {
        let directory = tempdir().unwrap();
        let ip = directory.path().join("ip");
        let log = directory.path().join("ip.log");
        let shell = command_in_path("bash");
        std::fs::write(
            &ip,
            format!(
                "#!{}\nprintf '%s\\n' \"$*\" >> '{}'\n",
                shell.display(),
                log.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&ip, std::fs::Permissions::from_mode(0o700)).unwrap();

        let manager = NetworkManager::new(&ip, "/fake/nsenter").unwrap();
        manager
            .create_named(&PodNetwork::for_build(), BUILD_NETWORK_NAMESPACE)
            .await
            .unwrap();
        let commands = std::fs::read_to_string(log).unwrap();
        let commands = commands.lines().collect::<Vec<_>>();
        assert_eq!(
            &commands[..3],
            [
                "link delete dev wbb00000000",
                "netns delete tascarrel-build",
                "netns add tascarrel-build",
            ]
        );
        assert!(commands.contains(&"link set wpb00000000 netns tascarrel-build"));
        assert!(commands.contains(&"-n tascarrel-build link set dev wpb00000000 name eth0"));
        assert!(
            commands
                .contains(&"-n tascarrel-build route replace default via 10.127.255.253 dev eth0")
        );
    }

    #[test]
    fn command_paths_must_be_absolute() {
        assert!(NetworkManager::new("ip", "/usr/bin/nsenter").is_err());
        assert!(NetworkManager::new("/usr/bin/ip", "nsenter").is_err());
    }
}
