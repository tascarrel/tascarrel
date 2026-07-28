//! OCI bundle construction and runc lifecycle operations for pods.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;
use std::fs::DirBuilder;
use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::io::Read;
use std::io::Write;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::time::Duration;

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;
use thiserror::Error;

use super::CommandOutput;
use super::CommandRunner;
use super::ImageConfig;
use super::ImageUser;
use super::PodId;
use super::PodStorage;
use super::ProcessCommandRunner;

/// Size of the primary ID range containing image users and groups.
pub const ID_MAP_SIZE: u32 = 65_536;
/// Size of a pod's outer ID map, including its subordinate-ID range.
pub const POD_ID_MAP_SIZE: u32 = ID_MAP_SIZE * 2;

const CONFIG_FILE: &str = "config.json";
const BUNDLE_DIRECTORY: &str = "bundle";
const MOUNTS_DIRECTORY: &str = "mounts";
const USER_NAMESPACE_FILE: &str = "userns";
const MOUNT_NAMESPACE_FILE: &str = "mountns";
const ROOTFS_MOUNT: &str = "rootfs";
const WORKSPACE_MOUNT: &str = "workspace";
const DOCKER_MOUNT: &str = "docker";
const TEMPORARY_MOUNT: &str = "temporary";
const RESOLV_CONF_FILE: &str = "resolv.conf";
const HOSTS_FILE: &str = "hosts";
const SUBUID_FILE: &str = "subuid";
const SUBGID_FILE: &str = "subgid";
const CONTAINERS_POLICY_FILE: &str = "containers-policy.json";
const PODMAN_PROGRAM_DESTINATION: &str = "/usr/local/bin/podman";
const NEWUIDMAP_PROGRAM_DESTINATION: &str = "/usr/local/bin/newuidmap";
const NEWGIDMAP_PROGRAM_DESTINATION: &str = "/usr/local/bin/newgidmap";
const CONTAINERS_POLICY_DESTINATION: &str = "/etc/containers/policy.json";
const RUNC_CREATE_LOG_FILE: &str = "runc-create.log";
const STARTUP_LOG_FILE: &str = "startup.log";
const USB_DEVICES_FILE: &str = "usb-devices.json";
const READINESS_DIRECTORY: &str = "readiness";
const READINESS_SOCKET_FILE: &str = "podd.sock";
const READINESS_SOCKET_DESTINATION: &str = "/run/tascarrel/guestd-readiness";
const READINESS_HANDSHAKE_PREFIX: &str = "TSRD01";
const READINESS_NONCE_BYTES: usize = 16;
const READINESS_HANDSHAKE_BYTES: usize = READINESS_HANDSHAKE_PREFIX.len() + 32;
const HOST_DEV_MOUNT: &str = "/run/tascarrel/host-dev";
/// Guest-owned devtmpfs directory containing only workspace-approved devices.
pub const POD_DEVICE_SOURCE_ROOT: &str = "/dev/.tascarrel-usb";
const RESOLV_CONF: &[u8] = b"nameserver 192.0.2.53\noptions edns0\n";
const HOSTS: &[u8] = b"127.0.0.1 localhost\n192.0.2.54 host.tascarrel.internal\n";
// Match the default containers/image policy shipped by NixOS. Tascarrel does
// not configure an image-signing trust root, so Podman must not imply one.
const CONTAINERS_POLICY: &[u8] = b"{\"default\":[{\"type\":\"insecureAcceptAnything\"}]}\n";
const TRUE_PROGRAM: &str = "/run/current-system/sw/bin/true";
const INITIAL_MOUNT_NAMESPACE_ROOT: &str = "/proc/1/root";
const MOUNTINFO: &str = "/proc/self/mountinfo";
const COMMAND_DIAGNOSTIC_LIMIT: usize = 4096;
const STARTUP_LOG_LIMIT: usize = 64 * 1024;
const RUNC_CREATE_TIMEOUT: Duration = Duration::from_mins(2);
const DOCKER_DAEMON: u8 = 1 << 0;
const NIX_DAEMON: u8 = 1 << 1;
const VIRTUALIZATION: u8 = 1 << 2;
const PODMAN: u8 = 1 << 3;
const KVM_DEVICE_MAJOR: i64 = 10;
const KVM_DEVICE_MINOR: i64 = 232;
const STANDARD_APPARMOR_PROFILE: &str = "tascarrel-pod";
const CONTAINER_APPARMOR_PROFILE: &str = "tascarrel-pod-containers";

const STANDARD_PODD_CAPABILITIES: &[&str] = &[
    "CAP_CHOWN",
    "CAP_DAC_OVERRIDE",
    "CAP_FOWNER",
    "CAP_KILL",
    "CAP_SETGID",
    "CAP_SETUID",
];

// These capabilities remain scoped by the mandatory outer user namespace.
// Capabilities which can affect global kernel state are deliberately absent.
const DOCKER_PODD_CAPABILITIES: &[&str] = &[
    "CAP_AUDIT_WRITE",
    "CAP_CHOWN",
    "CAP_DAC_OVERRIDE",
    "CAP_DAC_READ_SEARCH",
    "CAP_FOWNER",
    "CAP_FSETID",
    "CAP_IPC_LOCK",
    "CAP_IPC_OWNER",
    "CAP_KILL",
    "CAP_LEASE",
    "CAP_LINUX_IMMUTABLE",
    "CAP_MKNOD",
    "CAP_NET_ADMIN",
    "CAP_NET_BIND_SERVICE",
    "CAP_NET_BROADCAST",
    "CAP_NET_RAW",
    "CAP_SETFCAP",
    "CAP_SETGID",
    "CAP_SETPCAP",
    "CAP_SETUID",
    "CAP_SYS_ADMIN",
    "CAP_SYS_CHROOT",
    "CAP_SYS_NICE",
    "CAP_SYS_PTRACE",
    "CAP_SYS_RESOURCE",
    "CAP_SYS_TTY_CONFIG",
];

const FORBIDDEN_CAPABILITIES: &[&str] = &["CAP_SYS_BOOT", "CAP_SYS_MODULE", "CAP_SYS_TIME"];

// These operations mutate global kernel state and are not required by the
// supported pod features. User namespaces already reject most of them; the
// filter keeps that invariant independent of individual capability checks.
const GLOBAL_BLOCKED_SYSCALLS: &[&str] = &[
    "acct",
    "delete_module",
    "finit_module",
    "init_module",
    "kexec_file_load",
    "kexec_load",
    "reboot",
    "settimeofday",
    "swapoff",
    "swapon",
];

// Ordinary pods do not need to assemble another container mount namespace.
// Docker and Podman remove this second rule while retaining the global one.
const CONTAINER_BLOCKED_SYSCALLS: &[&str] = &[
    "fsconfig",
    "fsmount",
    "fsopen",
    "fspick",
    "mount",
    "mount_setattr",
    "move_mount",
    "open_tree",
    "pivot_root",
    "setns",
    "umount2",
    "unshare",
];

/// Workspace policy applied uniformly to every pod runtime in a VM.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PodPolicy(u8);

impl PodPolicy {
    /// Controls the opt-in virtualization feature.
    #[must_use]
    pub const fn with_virtualization(mut self, enabled: bool) -> Self {
        self.set(VIRTUALIZATION, enabled);
        self
    }

    /// Controls the rootful Docker daemon and injected client.
    #[must_use]
    pub const fn with_docker_daemon(mut self, enabled: bool) -> Self {
        self.set(DOCKER_DAEMON, enabled);
        self
    }

    /// Controls exposure of the VM's Nix daemon socket.
    #[must_use]
    pub const fn with_nix_daemon(mut self, enabled: bool) -> Self {
        self.set(NIX_DAEMON, enabled);
        self
    }

    /// Controls the injected rootless Podman feature.
    #[must_use]
    pub const fn with_podman(mut self, enabled: bool) -> Self {
        self.set(PODMAN, enabled);
        self
    }

    /// Returns whether rootless-container facilities are enabled.
    #[must_use]
    pub const fn rootless_containers(self) -> bool {
        self.podman()
    }

    /// Returns whether Docker needs the nested-container runtime surface.
    #[must_use]
    pub const fn nested_containers(self) -> bool {
        self.docker_daemon()
    }

    /// Returns whether pods receive the workspace VM's KVM device.
    #[must_use]
    pub const fn virtualization(self) -> bool {
        self.contains(VIRTUALIZATION)
    }

    /// Returns whether the Docker daemon service is enabled.
    #[must_use]
    pub const fn docker_daemon(self) -> bool {
        self.contains(DOCKER_DAEMON)
    }

    /// Returns whether the Nix daemon service is enabled.
    #[must_use]
    pub const fn nix_daemon(self) -> bool {
        self.contains(NIX_DAEMON)
    }

    /// Returns whether the Podman client feature is enabled.
    #[must_use]
    pub const fn podman(self) -> bool {
        self.contains(PODMAN)
    }

    /// Returns the `AppArmor` profile required by the enabled pod features.
    #[must_use]
    pub const fn apparmor_profile(self) -> &'static str {
        if self.nested_containers() || self.rootless_containers() {
            CONTAINER_APPARMOR_PROFILE
        } else {
            STANDARD_APPARMOR_PROFILE
        }
    }

    const fn contains(self, mask: u8) -> bool {
        self.0 & mask != 0
    }

    const fn set(&mut self, mask: u8, enabled: bool) {
        if enabled {
            self.0 |= mask;
        } else {
            self.0 &= !mask;
        }
    }
}

/// Linux device-node kind exposed inside a pod.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PodDeviceKind {
    /// Character device.
    Char,
    /// Block device.
    Block,
}

/// One validated VM device exposed at a path in every pod's private `/dev`.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct PodDevice {
    path: PathBuf,
    source: PathBuf,
    kind: PodDeviceKind,
    major: u32,
    minor: u32,
}

impl PodDevice {
    /// Creates a device destination below `/dev`.
    ///
    /// # Errors
    ///
    /// Returns an error for non-absolute, non-normal, or non-device paths.
    pub fn new(
        path: impl Into<PathBuf>,
        kind: PodDeviceKind,
        major: u32,
        minor: u32,
    ) -> Result<Self, RuntimeError> {
        let path = path.into();
        let device = Self {
            source: path.clone(),
            path,
            kind,
            major,
            minor,
        };
        device.validate()?;
        Ok(device)
    }

    /// Creates a pod-visible alias backed by an existing VM device node.
    ///
    /// # Errors
    ///
    /// Returns an error when either path is not a normal absolute child of
    /// `/dev`.
    pub fn from_source(
        path: impl Into<PathBuf>,
        source: impl Into<PathBuf>,
        kind: PodDeviceKind,
        major: u32,
        minor: u32,
    ) -> Result<Self, RuntimeError> {
        let device = Self {
            path: path.into(),
            source: source.into(),
            kind,
            major,
            minor,
        };
        device.validate()?;
        Ok(device)
    }

    /// Returns the pod-visible absolute device path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns the absolute VM device path backing the pod-visible name.
    #[must_use]
    pub fn source(&self) -> &Path {
        &self.source
    }

    /// Returns whether this is a character or block device.
    #[must_use]
    pub const fn kind(&self) -> PodDeviceKind {
        self.kind
    }

    /// Returns the Linux device major number.
    #[must_use]
    pub const fn major(&self) -> u32 {
        self.major
    }

    /// Returns the Linux device minor number.
    #[must_use]
    pub const fn minor(&self) -> u32 {
        self.minor
    }

    fn validate(&self) -> Result<(), RuntimeError> {
        for (label, path) in [
            ("pod device path", &self.path),
            ("VM device source", &self.source),
        ] {
            if !path.starts_with("/dev")
                || path == Path::new("/dev")
                || path.components().any(|component| {
                    matches!(
                        component,
                        Component::CurDir | Component::ParentDir | Component::Prefix(_)
                    )
                })
            {
                return Err(RuntimeError::InvalidConfig(format!(
                    "{label} must be a normal absolute child of /dev: {}",
                    path.display()
                )));
            }
        }
        Ok(())
    }
}

/// Validated storage paths mounted into one OCI bundle.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PodMounts {
    root: PathBuf,
    workspace: PathBuf,
    docker: PathBuf,
    temporary: PathBuf,
}

/// One guest-managed persistent share mounted read/write into every pod in a
/// workspace VM.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PodShare {
    name: String,
    source: PathBuf,
    path: String,
    read_only: bool,
    runtime_origin: bool,
    recursive_bind: bool,
}

impl PodShare {
    /// Creates a share from its stable backing name, source subvolume, and pod
    /// destination. Destinations may be absolute or use `~`/`~/...`; home
    /// expansion is deferred until the pinned image configuration is known.
    ///
    /// # Errors
    ///
    /// Returns an error for an unsafe name, source, or destination expression.
    pub fn new(
        name: impl Into<String>,
        source: impl Into<PathBuf>,
        path: impl Into<String>,
    ) -> Result<Self, RuntimeError> {
        let share = Self {
            name: name.into(),
            source: source.into(),
            path: path.into(),
            read_only: false,
            runtime_origin: false,
            recursive_bind: false,
        };
        share.validate()?;
        Ok(share)
    }

    /// Creates the runtime-owned, read-only workspace HTTPS CA mount.
    ///
    /// # Errors
    ///
    /// Returns an error when the source or fixed destination violates share
    /// path and isolation constraints.
    pub fn workspace_authority(source: impl Into<PathBuf>) -> Result<Self, RuntimeError> {
        let share = Self {
            name: "workspace-authority".to_owned(),
            source: source.into(),
            path: "/run/tascarrel/https-ca".to_owned(),
            read_only: true,
            runtime_origin: true,
            recursive_bind: false,
        };
        share.validate()?;
        Ok(share)
    }

    /// Creates the runtime-owned, read-only agent harness cache mounted into
    /// every pod.
    ///
    /// # Errors
    ///
    /// Returns an error when the source or reserved destination is unsafe.
    pub fn agent_harnesses(source: impl Into<PathBuf>) -> Result<Self, RuntimeError> {
        let share = Self {
            name: "agent-harnesses".to_owned(),
            source: source.into(),
            path: "/opt/tascarrel/harnesses".to_owned(),
            read_only: true,
            runtime_origin: true,
            recursive_bind: false,
        };
        share.validate()?;
        Ok(share)
    }

    /// Creates the pod-user-readable, read-only chat attachment store mounted
    /// into every pod.
    ///
    /// # Errors
    ///
    /// Returns an error when the source or reserved destination is unsafe.
    pub fn chat_attachments(source: impl Into<PathBuf>) -> Result<Self, RuntimeError> {
        let share = Self {
            name: "chat-attachments".to_owned(),
            source: source.into(),
            path: "/opt/tascarrel/chat/attachments".to_owned(),
            read_only: true,
            runtime_origin: false,
            recursive_bind: false,
        };
        share.validate()?;
        Ok(share)
    }

    /// Creates the runtime-owned, read-only code-server distribution.
    ///
    /// # Errors
    ///
    /// Returns an error when the source or reserved destination is unsafe.
    pub fn code_server(source: impl Into<PathBuf>) -> Result<Self, RuntimeError> {
        let share = Self {
            name: "code-server".to_owned(),
            source: source.into(),
            path: "/opt/tascarrel/tools/code-server".to_owned(),
            read_only: true,
            runtime_origin: true,
            recursive_bind: false,
        };
        share.validate()?;
        Ok(share)
    }

    /// Creates the runtime-owned read-only lifecycle hook generation.
    ///
    /// # Errors
    ///
    /// Returns an error when the source or reserved destination is unsafe.
    pub fn workspace_hooks(source: impl Into<PathBuf>) -> Result<Self, RuntimeError> {
        let share = Self {
            name: "workspace-hooks".to_owned(),
            source: source.into(),
            path: "/run/tascarrel/hooks".to_owned(),
            read_only: true,
            runtime_origin: true,
            recursive_bind: false,
        };
        share.validate()?;
        Ok(share)
    }

    /// Creates the runtime-owned, read-only workspace agent configuration.
    ///
    /// Harness adapters can consume the common files below
    /// `/run/tascarrel/agents` without coupling the pod runtime to one harness.
    ///
    /// # Errors
    ///
    /// Returns an error when the source or reserved destination is unsafe.
    pub fn workspace_agents(source: impl Into<PathBuf>) -> Result<Self, RuntimeError> {
        let share = Self {
            name: "workspace-agents".to_owned(),
            source: source.into(),
            path: "/run/tascarrel/agents".to_owned(),
            read_only: true,
            runtime_origin: true,
            recursive_bind: false,
        };
        share.validate()?;
        Ok(share)
    }

    /// Creates a read-only user-scoped agent skill directory from workspace
    /// inputs.
    ///
    /// # Errors
    ///
    /// Returns an error when the source or home-relative destination is unsafe.
    pub fn workspace_agent_skills(source: impl Into<PathBuf>) -> Result<Self, RuntimeError> {
        let share = Self {
            name: "workspace-agent-skills".to_owned(),
            source: source.into(),
            path: "~/.agents/skills".to_owned(),
            read_only: true,
            runtime_origin: true,
            recursive_bind: false,
        };
        share.validate()?;
        Ok(share)
    }

    fn validate(&self) -> Result<(), RuntimeError> {
        PodId::new(&self.name).map_err(|error| RuntimeError::InvalidConfig(error.to_string()))?;
        validate_absolute_path(&self.source, "workspace share source")?;
        validate_share_path_expression(&self.path)?;
        Ok(())
    }
}

impl PodMounts {
    /// Constructs a set of distinct absolute storage paths.
    ///
    /// The paths are checked for real, non-symlink directories again directly
    /// before mounting them.
    ///
    /// # Errors
    ///
    /// Returns an error for relative, non-normal, or duplicate paths.
    pub fn new(
        root: impl Into<PathBuf>,
        workspace: impl Into<PathBuf>,
        docker: impl Into<PathBuf>,
        temporary: impl Into<PathBuf>,
    ) -> Result<Self, RuntimeError> {
        let mounts = Self {
            root: root.into(),
            workspace: workspace.into(),
            docker: docker.into(),
            temporary: temporary.into(),
        };
        for path in [
            &mounts.root,
            &mounts.workspace,
            &mounts.docker,
            &mounts.temporary,
        ] {
            validate_absolute_path(path, "pod storage path")?;
        }
        if mounts.root == mounts.workspace
            || mounts.root == mounts.docker
            || mounts.root == mounts.temporary
            || mounts.workspace == mounts.docker
            || mounts.workspace == mounts.temporary
            || mounts.docker == mounts.temporary
        {
            return Err(RuntimeError::InvalidConfig(
                "pod root, workspace, Docker, and temporary storage must be distinct".to_owned(),
            ));
        }
        Ok(mounts)
    }

    /// Returns the writable pod root snapshot.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Returns the independent workspace subvolume.
    #[must_use]
    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    /// Returns the independent Docker data subvolume.
    #[must_use]
    pub fn docker(&self) -> &Path {
        &self.docker
    }

    /// Returns the independent temporary subvolume.
    #[must_use]
    pub fn temporary(&self) -> &Path {
        &self.temporary
    }
}

impl TryFrom<&PodStorage> for PodMounts {
    type Error = RuntimeError;

    fn try_from(storage: &PodStorage) -> Result<Self, Self::Error> {
        Self::new(
            storage.root(),
            storage.workspace(),
            storage.docker(),
            storage.temporary(),
        )
    }
}

/// Immutable Nix store programs injected into every pod root.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PodPrograms {
    nix_store: PathBuf,
    podd: PathBuf,
    podctl: PathBuf,
    tasci: PathBuf,
    shell: PathBuf,
    terminal_shell: PathBuf,
    dockerd: PathBuf,
    docker: PathBuf,
    podman: PathBuf,
    newuidmap: PathBuf,
    newgidmap: PathBuf,
    nix: PathBuf,
}

impl PodPrograms {
    /// Groups the immutable Nix store paths needed by the runtime.
    ///
    /// # Errors
    ///
    /// Returns an error unless all program paths are absolute normalized
    /// children of `nix_store`.
    #[expect(
        clippy::too_many_arguments,
        reason = "the immutable program bundle keeps every injected executable explicit"
    )]
    pub fn new(
        nix_store: impl Into<PathBuf>,
        podd: impl Into<PathBuf>,
        podctl: impl Into<PathBuf>,
        tasci: impl Into<PathBuf>,
        shell: impl Into<PathBuf>,
        terminal_shell: impl Into<PathBuf>,
        dockerd: impl Into<PathBuf>,
        docker: impl Into<PathBuf>,
        podman: impl Into<PathBuf>,
        user_mapping_helper: impl Into<PathBuf>,
        group_mapping_helper: impl Into<PathBuf>,
        nix: impl Into<PathBuf>,
    ) -> Result<Self, RuntimeError> {
        let programs = Self {
            nix_store: nix_store.into(),
            podd: podd.into(),
            podctl: podctl.into(),
            tasci: tasci.into(),
            shell: shell.into(),
            terminal_shell: terminal_shell.into(),
            dockerd: dockerd.into(),
            docker: docker.into(),
            podman: podman.into(),
            newuidmap: user_mapping_helper.into(),
            newgidmap: group_mapping_helper.into(),
            nix: nix.into(),
        };
        programs.validate()?;
        Ok(programs)
    }

    /// Returns the immutable shell injected into every pod.
    #[must_use]
    pub fn shell(&self) -> &Path {
        &self.shell
    }

    /// Returns the immutable default interactive shell injected into every pod.
    #[must_use]
    pub fn terminal_shell(&self) -> &Path {
        &self.terminal_shell
    }

    fn validate(&self) -> Result<(), RuntimeError> {
        validate_absolute_path(&self.nix_store, "Nix store")?;
        for (program, name) in [
            (&self.podd, "podd"),
            (&self.podctl, "podctl"),
            (&self.tasci, "Tasci harness"),
            (&self.shell, "setup shell"),
            (&self.terminal_shell, "terminal shell"),
            (&self.dockerd, "dockerd"),
            (&self.docker, "docker client"),
            (&self.podman, "Podman client"),
            (&self.newuidmap, "newuidmap helper"),
            (&self.newgidmap, "newgidmap helper"),
            (&self.nix, "Nix client"),
        ] {
            validate_absolute_path(program, &format!("{name} program"))?;
            if !program.starts_with(&self.nix_store) || program == &self.nix_store {
                return Err(RuntimeError::InvalidConfig(format!(
                    "{name} must use its immutable path below the configured Nix store"
                )));
            }
        }
        Ok(())
    }
}

/// Paths and ID allocation used by the OCI runtime.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeConfig {
    runtime_root: PathBuf,
    runc_root: PathBuf,
    runc_program: PathBuf,
    mount_program: PathBuf,
    umount_program: PathBuf,
    unshare_program: PathBuf,
    nsenter_program: PathBuf,
    ip_program: PathBuf,
    nix_store: PathBuf,
    podd_program: PathBuf,
    podctl_program: PathBuf,
    tasci_program: PathBuf,
    shell_program: PathBuf,
    terminal_shell_program: PathBuf,
    dockerd_program: PathBuf,
    docker_program: PathBuf,
    podman_program: PathBuf,
    newuidmap_program: PathBuf,
    newgidmap_program: PathBuf,
    nix_program: PathBuf,
    uid_base: u32,
    gid_base: u32,
    cgroup_parent: String,
    systemd_cgroup: bool,
    policy: PodPolicy,
    pod_nix_store: PathBuf,
    nix_daemon_socket_directory: PathBuf,
    nix_gc_root_directory: PathBuf,
    pod_nix_gc_root_directory: PathBuf,
    shares: Vec<PodShare>,
    devices: Vec<PodDevice>,
}

impl RuntimeConfig {
    /// Constructs a configuration using the stable system-profile paths for
    /// the low-level tools.
    ///
    /// All injected programs must name immutable paths below
    /// `nix_store`, not mutable profile symlinks. `shell_program` is bind
    /// mounted at `/bin/sh`, so even a `FROM scratch` image remains operable.
    /// Both host ID ranges contain the primary 65,536 IDs plus a second
    /// 65,536-ID range delegated to the image user for subordinate IDs. The
    /// ranges may not include host ID zero.
    ///
    /// # Errors
    ///
    /// Returns an error for unsafe paths or ID ranges.
    pub fn new(
        runtime_root: impl Into<PathBuf>,
        runc_root: impl Into<PathBuf>,
        programs: PodPrograms,
        uid_base: u32,
        gid_base: u32,
    ) -> Result<Self, RuntimeError> {
        let Some(nix_prefix) = programs.nix_store.parent() else {
            return Err(RuntimeError::InvalidConfig(
                "Nix store must have an absolute parent".to_owned(),
            ));
        };
        let nix_daemon_socket_directory = nix_prefix.join("var/nix/daemon-socket");
        let nix_gc_root_directory = nix_prefix.join("var/nix/gcroots/tascarrel/pods");
        let pod_nix_store = programs.nix_store.clone();
        let config = Self {
            runtime_root: runtime_root.into(),
            runc_root: runc_root.into(),
            runc_program: PathBuf::from("/run/current-system/sw/bin/runc"),
            mount_program: PathBuf::from("/run/current-system/sw/bin/mount"),
            umount_program: PathBuf::from("/run/current-system/sw/bin/umount"),
            unshare_program: PathBuf::from("/run/current-system/sw/bin/unshare"),
            nsenter_program: PathBuf::from("/run/current-system/sw/bin/nsenter"),
            ip_program: PathBuf::from("/run/current-system/sw/bin/ip"),
            nix_store: programs.nix_store,
            podd_program: programs.podd,
            podctl_program: programs.podctl,
            tasci_program: programs.tasci,
            shell_program: programs.shell,
            terminal_shell_program: programs.terminal_shell,
            dockerd_program: programs.dockerd,
            docker_program: programs.docker,
            podman_program: programs.podman,
            newuidmap_program: programs.newuidmap,
            newgidmap_program: programs.newgidmap,
            nix_program: programs.nix,
            uid_base,
            gid_base,
            cgroup_parent: "tascarrel".to_owned(),
            systemd_cgroup: false,
            policy: PodPolicy::default(),
            pod_nix_store,
            nix_daemon_socket_directory,
            pod_nix_gc_root_directory: nix_gc_root_directory.clone(),
            nix_gc_root_directory,
            shares: Vec::new(),
            devices: Vec::new(),
        };
        config.validate()?;
        Ok(config)
    }

    /// Overrides all external tools. Every path must remain absolute.
    ///
    /// # Errors
    ///
    /// Returns an error for a relative or non-normal path.
    pub fn with_programs(
        mut self,
        runc: impl Into<PathBuf>,
        mount: impl Into<PathBuf>,
        umount: impl Into<PathBuf>,
        unshare: impl Into<PathBuf>,
        nsenter: impl Into<PathBuf>,
        ip: impl Into<PathBuf>,
    ) -> Result<Self, RuntimeError> {
        self.runc_program = runc.into();
        self.mount_program = mount.into();
        self.umount_program = umount.into();
        self.unshare_program = unshare.into();
        self.nsenter_program = nsenter.into();
        self.ip_program = ip.into();
        self.validate()?;
        Ok(self)
    }

    /// Selects the safe single-component parent for generated cgroup paths.
    ///
    /// # Errors
    ///
    /// Returns an error unless the value uses the pod-ID alphabet.
    pub fn with_cgroup_parent(mut self, parent: impl Into<String>) -> Result<Self, RuntimeError> {
        let parent = parent.into();
        PodId::new(parent.clone())
            .map_err(|error| RuntimeError::InvalidConfig(error.to_string()))?;
        self.cgroup_parent = parent;
        Ok(self)
    }

    /// Enables runc's systemd cgroup manager.
    #[must_use]
    pub const fn with_systemd_cgroup(mut self, enabled: bool) -> Self {
        self.systemd_cgroup = enabled;
        self
    }

    /// Applies one validated workspace policy to the runtime.
    #[must_use]
    pub const fn with_policy(mut self, policy: PodPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Configures the physical persistent Nix service exposed to pods.
    ///
    /// `store` and `gc_root_directory` name physical guest paths. The store is
    /// presented at `/nix/store`, while only the requested pod's direct-root
    /// child is mounted at the corresponding child of
    /// `pod_gc_root_directory`. The guest lifecycle manager must provision
    /// that physical child before `create` is called.
    ///
    /// # Errors
    ///
    /// Returns an error for a relative or non-normal path.
    pub fn with_nix_service(
        mut self,
        store: impl Into<PathBuf>,
        socket_directory: impl Into<PathBuf>,
        gc_root_directory: impl Into<PathBuf>,
        pod_gc_root_directory: impl Into<PathBuf>,
    ) -> Result<Self, RuntimeError> {
        self.pod_nix_store = store.into();
        self.nix_daemon_socket_directory = socket_directory.into();
        self.nix_gc_root_directory = gc_root_directory.into();
        self.pod_nix_gc_root_directory = pod_gc_root_directory.into();
        self.validate()?;
        Ok(self)
    }

    /// Configures the persistent workspace shares mounted into every pod.
    ///
    /// # Errors
    ///
    /// Returns an error for duplicate names/sources or invalid definitions.
    pub fn with_shares(
        mut self,
        shares: impl IntoIterator<Item = PodShare>,
    ) -> Result<Self, RuntimeError> {
        self.shares = shares.into_iter().collect();
        self.validate()?;
        Ok(self)
    }

    /// Configures workspace hardware nodes present when the pod is created.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid or duplicate destinations.
    pub fn with_devices(
        mut self,
        devices: impl IntoIterator<Item = PodDevice>,
    ) -> Result<Self, RuntimeError> {
        self.devices = devices.into_iter().collect();
        self.validate()?;
        Ok(self)
    }

    /// Returns the direct-root path reserved for one pod.
    #[must_use]
    pub fn nix_gc_root_path(&self, pod: &PodId) -> PathBuf {
        self.nix_gc_root_directory.join(pod.as_str())
    }

    /// Returns the direct-root path visible inside one pod.
    #[must_use]
    pub fn pod_nix_gc_root_path(&self, pod: &PodId) -> PathBuf {
        self.pod_nix_gc_root_directory.join(pod.as_str())
    }

    fn validate(&self) -> Result<(), RuntimeError> {
        for (path, purpose) in [
            (&self.runtime_root, "runtime root"),
            (&self.runc_root, "runc state root"),
            (&self.runc_program, "runc program"),
            (&self.mount_program, "mount program"),
            (&self.umount_program, "umount program"),
            (&self.unshare_program, "unshare program"),
            (&self.nsenter_program, "nsenter program"),
            (&self.ip_program, "ip program"),
            (&self.nix_store, "Nix store"),
            (&self.pod_nix_store, "persistent pod Nix store"),
            (&self.podd_program, "podd program"),
            (&self.podctl_program, "podctl program"),
            (&self.shell_program, "shell program"),
            (&self.terminal_shell_program, "terminal shell program"),
            (&self.dockerd_program, "dockerd program"),
            (
                &self.nix_daemon_socket_directory,
                "Nix daemon socket directory",
            ),
            (&self.nix_gc_root_directory, "Nix pod GC-root directory"),
            (
                &self.pod_nix_gc_root_directory,
                "pod-visible Nix GC-root directory",
            ),
        ] {
            validate_absolute_path(path, purpose)?;
        }
        let mut share_names = BTreeSet::new();
        let mut share_sources = BTreeSet::new();
        for share in &self.shares {
            share.validate()?;
            if !share_names.insert(&share.name) {
                return Err(RuntimeError::InvalidConfig(format!(
                    "duplicate workspace share name {:?}",
                    share.name
                )));
            }
            if !share_sources.insert(&share.source) {
                return Err(RuntimeError::InvalidConfig(format!(
                    "duplicate workspace share source {}",
                    share.source.display()
                )));
            }
        }
        PodPrograms {
            nix_store: self.nix_store.clone(),
            podd: self.podd_program.clone(),
            podctl: self.podctl_program.clone(),
            tasci: self.tasci_program.clone(),
            shell: self.shell_program.clone(),
            terminal_shell: self.terminal_shell_program.clone(),
            dockerd: self.dockerd_program.clone(),
            docker: self.docker_program.clone(),
            podman: self.podman_program.clone(),
            newuidmap: self.newuidmap_program.clone(),
            newgidmap: self.newgidmap_program.clone(),
            nix: self.nix_program.clone(),
        }
        .validate()?;
        validate_id_base(self.uid_base, "UID")?;
        validate_id_base(self.gid_base, "GID")?;
        PodId::new(self.cgroup_parent.clone())
            .map_err(|error| RuntimeError::InvalidConfig(error.to_string()))?;
        let mut device_paths = BTreeSet::new();
        for device in &self.devices {
            device.validate()?;
            if !device_paths.insert(device.path()) {
                return Err(RuntimeError::InvalidConfig(format!(
                    "duplicate pod device path {}",
                    device.path().display()
                )));
            }
        }
        Ok(())
    }
}

/// Failure from OCI bundle preparation or a runc lifecycle operation.
#[derive(Debug, Error)]
pub enum RuntimeError {
    /// Static runtime configuration was unsafe or inconsistent.
    #[error("invalid runtime configuration: {0}")]
    InvalidConfig(String),
    /// A managed path was a symlink or had an unexpected file type.
    #[error("unsafe runtime path: {0}")]
    UnsafePath(PathBuf),
    /// The runtime already has local state for the requested pod.
    #[error("pod {0} already has prepared runtime state")]
    AlreadyPrepared(PodId),
    /// The runtime has no local state for the requested pod.
    #[error("pod {0} has no prepared runtime state")]
    NotPrepared(PodId),
    /// A filesystem operation failed.
    #[error("could not {operation} {path}: {source}")]
    Io {
        /// Description of the attempted operation.
        operation: &'static str,
        /// Affected path.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: io::Error,
    },
    /// An OCI configuration or runc state document was invalid.
    #[error("invalid JSON at {path}: {source}")]
    Json {
        /// Affected path or logical command output.
        path: PathBuf,
        /// JSON failure.
        #[source]
        source: serde_json::Error,
    },
    /// An external executable could not be started.
    #[error("could not start {program} while attempting to {operation}: {source}")]
    CommandStart {
        /// Logical operation.
        operation: &'static str,
        /// Absolute executable path.
        program: PathBuf,
        /// Process creation failure.
        #[source]
        source: io::Error,
    },
    /// An external executable rejected an operation.
    #[error("runtime operation `{operation}` failed: {detail}")]
    CommandFailed {
        /// Logical operation.
        operation: &'static str,
        /// Bounded diagnostic.
        detail: String,
    },
    /// runc returned a state document which cannot identify the pod's init.
    #[error("invalid runc state for pod {pod}: {reason}")]
    InvalidState {
        /// Expected pod.
        pod: PodId,
        /// Validation failure.
        reason: String,
    },
    /// Both an operation and its best-effort rollback failed.
    #[error("{operation} failed: {cause}; rollback also failed: {rollback}")]
    RollbackFailed {
        /// High-level operation being rolled back.
        operation: &'static str,
        /// Initial failure.
        cause: String,
        /// Rollback failure.
        rollback: String,
    },
    /// The lifecycle mutex was poisoned by a panic.
    #[error("pod runtime operation lock is poisoned")]
    LockPoisoned,
}

/// Parsed lifecycle state returned by runc.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContainerState {
    status: ContainerStatus,
    pid: Option<u32>,
}

impl ContainerState {
    /// Returns runc's normalized lifecycle status.
    #[must_use]
    pub const fn status(&self) -> &ContainerStatus {
        &self.status
    }

    /// Returns the init PID while the container has one.
    #[must_use]
    pub const fn pid(&self) -> Option<u32> {
        self.pid
    }

    /// Produces a handle to the pod's network namespace while it has an init
    /// process.
    #[must_use]
    pub fn network_namespace(&self) -> Option<NetworkNamespace> {
        self.pid.map(|pid| NetworkNamespace { pid })
    }
}

/// A normalized runc lifecycle status.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ContainerStatus {
    /// The init is still being assembled.
    Creating,
    /// `runc create` completed and the init is waiting for `runc start`.
    Created,
    /// The init is running.
    Running,
    /// The container has no live init.
    Stopped,
    /// All processes are frozen.
    Paused,
    /// A newer runc returned an unrecognized status.
    Unknown(String),
}

/// Result of preparing a pod before its init is started.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CreatedPod {
    network: NetworkNamespace,
    readiness_directory: PathBuf,
    readiness_handshake: String,
}

impl CreatedPod {
    /// Returns the created init's network namespace. The caller configures its
    /// veth before calling [`PodRuntime::start`].
    #[must_use]
    pub const fn network_namespace(&self) -> &NetworkNamespace {
        &self.network
    }

    /// Returns the host directory mounted only into this pod for its
    /// guestd-readiness socket.
    #[must_use]
    pub fn readiness_directory(&self) -> &Path {
        &self.readiness_directory
    }

    /// Returns the host pathname where guestd binds this attempt's listener.
    #[must_use]
    pub fn readiness_socket(&self) -> PathBuf {
        self.readiness_directory.join(READINESS_SOCKET_FILE)
    }

    /// Returns the fixed-size, versioned, per-attempt readiness handshake.
    #[must_use]
    pub fn readiness_handshake(&self) -> &str {
        &self.readiness_handshake
    }
}

/// Stable reference to one created pod network namespace.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NetworkNamespace {
    pid: u32,
}

impl NetworkNamespace {
    /// Returns the pod init PID.
    #[must_use]
    pub const fn pid(&self) -> u32 {
        self.pid
    }
}

/// Native Tascarrel pod lifecycle implemented through runc.
pub struct PodRuntime<R = ProcessCommandRunner> {
    config: RuntimeConfig,
    runner: Arc<R>,
    operation: Mutex<()>,
}

impl<R> std::fmt::Debug for PodRuntime<R> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PodRuntime")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl PodRuntime<ProcessCommandRunner> {
    /// Initializes private runtime directories using real child processes.
    ///
    /// # Errors
    ///
    /// Returns an error if configuration or runtime paths are unsafe.
    pub fn open(config: RuntimeConfig) -> Result<Self, RuntimeError> {
        Self::with_runner(config, ProcessCommandRunner)
    }
}

impl<R: CommandRunner> PodRuntime<R> {
    /// Initializes private runtime directories with an injected command
    /// runner.
    ///
    /// # Errors
    ///
    /// Returns an error if configuration or runtime paths are unsafe.
    pub fn with_runner(config: RuntimeConfig, runner: R) -> Result<Self, RuntimeError> {
        config.validate()?;
        // runc joins the pod user namespace before it finishes preparing the
        // root mount. Its mapped root has no DAC override over initial-userns
        // directories, so the transient path must remain searchable. Files
        // below it (including the OCI config and namespace pins) stay private.
        ensure_searchable_directory(&config.runtime_root)?;
        ensure_private_directory(&config.runc_root)?;
        Ok(Self {
            config,
            runner: Arc::new(runner),
            operation: Mutex::new(()),
        })
    }

    /// Prepares a pod from validated storage paths and retained OCI defaults.
    ///
    /// This accepts retained image metadata in addition to validated storage
    /// paths.
    ///
    /// # Errors
    ///
    /// Returns an error if paths are unsafe, mounting fails, the OCI bundle is
    /// rejected, or runc does not report a live created init.
    pub fn create_from_mounts_and_config(
        &self,
        pod: &PodId,
        storage: &PodMounts,
        image_config: &ImageConfig,
    ) -> Result<CreatedPod, RuntimeError> {
        let _operation = self.lock()?;
        self.check_static_sources(pod, storage)?;

        let paths = PodRuntimePaths::new(&self.config.runtime_root, pod);
        let shares = self.resolve_shares(image_config, &paths)?;
        if path_state(&paths.pod)?.is_some() {
            return Err(RuntimeError::AlreadyPrepared(pod.clone()));
        }
        prepare_share_destinations(storage, image_config, &shares)?;
        create_searchable_directory(&paths.pod)?;
        let (result, runc_attempted, mounted) =
            self.prepare_and_create(pod, storage, image_config, &paths, &shares);
        if let Err(cause) = result {
            let rollback = self.rollback_create(pod, &paths, runc_attempted, mounted);
            return match rollback {
                Ok(()) => Err(cause),
                Err(rollback) => Err(RuntimeError::RollbackFailed {
                    operation: "create pod",
                    cause: cause.to_string(),
                    rollback: rollback.to_string(),
                }),
            };
        }
        result
    }

    /// Starts the previously created pod init.
    ///
    /// # Errors
    ///
    /// Returns an error if the pod is not prepared or runc rejects the start.
    pub fn start(&self, pod: &PodId) -> Result<(), RuntimeError> {
        let _operation = self.lock()?;
        self.require_prepared(pod)?;
        self.run_runc("start pod", [OsString::from("start"), pod.as_str().into()])?;
        Ok(())
    }

    /// Returns the bounded stdout/stderr tail captured from pod init.
    ///
    /// # Errors
    ///
    /// Returns an error if the pod is not prepared or its log cannot be read.
    pub fn startup_log(&self, pod: &PodId) -> Result<Vec<u8>, RuntimeError> {
        let _operation = self.lock()?;
        self.require_prepared(pod)?;
        read_bounded_file(
            &PodRuntimePaths::new(&self.config.runtime_root, pod).startup_log,
            STARTUP_LOG_LIMIT,
        )
    }

    /// Replaces workspace hardware access for a running pod.
    ///
    /// Pods may read and write only device nodes already present in their
    /// private `/dev` tree. Device-node creation remains restricted to the
    /// runtime's static allowlist, so adding or removing a curated link is the
    /// complete hotplug operation and does not depend on unsupported dynamic
    /// device updates in runc.
    ///
    /// # Errors
    ///
    /// Returns an error when runc, namespace entry, or device materialization
    /// fails.
    #[allow(clippy::too_many_lines)] // Ordered policy and node updates keep revocation auditable.
    pub fn sync_devices(&self, pod: &PodId, devices: &[PodDevice]) -> Result<(), RuntimeError> {
        let _operation = self.lock()?;
        self.require_prepared(pod)?;
        for device in devices {
            device.validate()?;
        }
        let state = self.state_locked(pod)?;
        if state.status != ContainerStatus::Running {
            return Err(RuntimeError::InvalidState {
                pod: pod.clone(),
                reason: format!("cannot update devices while pod is {:?}", state.status),
            });
        }
        let pid = state.pid.ok_or_else(|| RuntimeError::InvalidState {
            pod: pod.clone(),
            reason: "running pod has no init PID".to_owned(),
        })?;
        let paths = PodRuntimePaths::new(&self.config.runtime_root, pod);
        let previous = read_device_manifest(&paths.usb_devices)?;
        for path in previous
            .iter()
            .map(PodDevice::path)
            .filter(|path| !devices.iter().any(|device| device.path() == *path))
        {
            self.run_in_pod_root(
                &paths,
                pid,
                "remove detached pod device",
                &[
                    OsString::from("device-remove"),
                    OsString::from("--path"),
                    path.as_os_str().to_owned(),
                ],
            )?;
        }
        self.link_pod_devices(&paths, pid, devices)?;
        write_json_replace(
            &paths.usb_devices,
            &serde_json::to_value(devices).map_err(|source| RuntimeError::Json {
                path: paths.usb_devices.clone(),
                source,
            })?,
        )
    }

    fn run_in_pod_root(
        &self,
        paths: &PodRuntimePaths,
        pid: u32,
        operation: &'static str,
        command: &[OsString],
    ) -> Result<(), RuntimeError> {
        let mut mount = OsString::from("--mount=");
        mount.push(paths.mount_namespace.as_os_str());
        let mut user = OsString::from("--user=");
        user.push(paths.user_namespace.as_os_str());
        let mut arguments = vec![
            OsString::from("--target"),
            OsString::from(pid.to_string()),
            user,
            mount,
            OsString::from("--root"),
            OsString::from("--wd"),
            OsString::from("--"),
            OsString::from("/usr/local/bin/podctl"),
        ];
        arguments.extend(command.iter().cloned());
        self.run(operation, &self.config.nsenter_program, &arguments)?;
        Ok(())
    }

    /// Reads and validates runc state.
    ///
    /// # Errors
    ///
    /// Returns an error if the pod is not prepared, runc fails, or its state
    /// document does not identify the requested pod.
    pub fn state(&self, pod: &PodId) -> Result<ContainerState, RuntimeError> {
        let _operation = self.lock()?;
        self.require_prepared(pod)?;
        self.state_locked(pod)
    }

    /// Deletes runc state, unmounts all idmapped mounts, and removes the
    /// private bundle directory.
    ///
    /// # Errors
    ///
    /// Returns an error if runc refuses deletion or cleanup cannot be safely
    /// completed. Local mounts are retained when runc refuses deletion.
    pub fn delete(&self, pod: &PodId, force: bool) -> Result<(), RuntimeError> {
        let _operation = self.lock()?;
        self.require_prepared(pod)?;
        let mut arguments = vec![OsString::from("delete")];
        if force {
            arguments.push(OsString::from("--force"));
        }
        arguments.push(pod.as_str().into());
        if let Err(delete_error) = self.run_runc("delete pod", arguments) {
            match self.runc_contains(pod) {
                // A previous attempt may have successfully removed runc state
                // and crashed before releasing the local mounts. Once runc's
                // complete list confirms that the ID is absent, continuing
                // cleanup is safe and makes destroy/recovery convergent.
                Ok(false) => {}
                Ok(true) => return Err(delete_error),
                Err(check_error) => {
                    return Err(RuntimeError::RollbackFailed {
                        operation: "verify failed pod deletion",
                        cause: delete_error.to_string(),
                        rollback: check_error.to_string(),
                    });
                }
            }
        }
        self.cleanup_local(&PodRuntimePaths::new(&self.config.runtime_root, pod))
    }

    /// Force-destroys a pod and releases all of its transient runtime mounts.
    /// Persistent Btrfs storage is intentionally left to
    /// [`crate::runtime::pod::BtrfsStore`].
    ///
    /// # Errors
    ///
    /// Returns an error if runc cannot terminate/delete the pod or a mount
    /// cannot be released.
    pub fn destroy(&self, pod: &PodId) -> Result<(), RuntimeError> {
        self.delete(pod, true)
    }

    fn prepare_and_create(
        &self,
        pod: &PodId,
        storage: &PodMounts,
        image_config: &ImageConfig,
        paths: &PodRuntimePaths,
        shares: &[ResolvedPodShare],
    ) -> (Result<CreatedPod, RuntimeError>, bool, usize) {
        let mut mounted = 0;
        let mut runc_attempted = false;
        let result = (|| {
            self.prepare_bundle_layout(image_config, paths, shares)?;

            write_file_exclusive(&paths.user_namespace, &[], 0o600)?;
            write_file_exclusive(&paths.mount_namespace, &[], 0o600)?;
            // util-linux pins the user namespace before the mount namespace.
            // Its command can therefore fail after creating only the first
            // nsfs bind; both candidate paths must enter rollback before it
            // is invoked.
            mounted = paths.cleanup_mountpoints().len();
            self.create_namespaces(&paths.user_namespace, &paths.mount_namespace)?;
            self.expose_user_namespace(&paths.user_namespace, &paths.mount_namespace)?;

            for (source, target) in [
                (&storage.root, &paths.rootfs),
                (&storage.workspace, &paths.workspace),
                (&storage.docker, &paths.docker),
                (&storage.temporary, &paths.temporary),
            ] {
                self.mount_idmapped(
                    source,
                    target,
                    &paths.user_namespace,
                    &paths.mount_namespace,
                )?;
            }
            for share in shares {
                if share.runtime_origin {
                    self.mount_bind(
                        &share.source,
                        &share.mountpoint,
                        &paths.mount_namespace,
                        share.recursive_bind,
                    )?;
                } else {
                    self.mount_idmapped(
                        &share.source,
                        &share.mountpoint,
                        &paths.user_namespace,
                        &paths.mount_namespace,
                    )?;
                }
            }

            let readiness_handshake = readiness_handshake()?;
            let configuration =
                self.oci_configuration(pod, image_config, paths, shares, &readiness_handshake)?;
            write_json_exclusive(&paths.bundle.join(CONFIG_FILE), &configuration)?;
            write_json_exclusive(
                &paths.usb_devices,
                &serde_json::to_value(&self.config.devices).map_err(|source| {
                    RuntimeError::Json {
                        path: paths.usb_devices.clone(),
                        source,
                    }
                })?,
            )?;
            write_file_exclusive(&paths.runc_create_log, &[], 0o600)?;
            write_file_exclusive(&paths.startup_log, &[], 0o600)?;
            // A failed runc create can still leave a live init or runtime
            // state. Mark the attempt before invocation so rollback proves
            // the ID absent before either namespace is unpinned.
            runc_attempted = true;
            self.run_runc_create(pod, paths)?;
            let state = self.state_locked(pod)?;
            if state.status != ContainerStatus::Created {
                return Err(RuntimeError::InvalidState {
                    pod: pod.clone(),
                    reason: format!("expected created status, got {:?}", state.status),
                });
            }
            let pid = state.pid.ok_or_else(|| RuntimeError::InvalidState {
                pod: pod.clone(),
                reason: "created state did not contain a live init PID".to_owned(),
            })?;
            self.link_pod_devices(paths, pid, &self.config.devices)?;
            let network = state
                .network_namespace()
                .ok_or_else(|| RuntimeError::InvalidState {
                    pod: pod.clone(),
                    reason: "created state did not contain a live init PID".to_owned(),
                })?;
            Ok(CreatedPod {
                network,
                readiness_directory: paths.readiness.clone(),
                readiness_handshake,
            })
        })();
        (result, runc_attempted, mounted)
    }

    /// Creates the private directories and policy files used by a pod bundle.
    fn prepare_bundle_layout(
        &self,
        image_config: &ImageConfig,
        paths: &PodRuntimePaths,
        shares: &[ResolvedPodShare],
    ) -> Result<(), RuntimeError> {
        create_searchable_directory(&paths.bundle)?;
        create_searchable_directory(&paths.mounts)?;
        create_private_directory(&paths.readiness)?;
        write_file_exclusive(&paths.resolv_conf, RESOLV_CONF, 0o644)?;
        write_file_exclusive(&paths.hosts, HOSTS, 0o644)?;
        if self.config.policy.podman() {
            let subordinate_ids = subordinate_id_file(image_config.user());
            write_file_exclusive(&paths.subuid, subordinate_ids.as_bytes(), 0o644)?;
            write_file_exclusive(&paths.subgid, subordinate_ids.as_bytes(), 0o644)?;
            write_file_exclusive(&paths.containers_policy, CONTAINERS_POLICY, 0o644)?;
        }
        for mountpoint in paths.mountpoints() {
            create_private_directory(mountpoint)?;
        }
        for share in shares {
            create_private_directory(&share.mountpoint)?;
        }
        Ok(())
    }

    fn link_pod_devices(
        &self,
        paths: &PodRuntimePaths,
        pid: u32,
        devices: &[PodDevice],
    ) -> Result<(), RuntimeError> {
        for device in devices {
            let relative_source = device.source().strip_prefix("/dev").map_err(|_| {
                RuntimeError::InvalidConfig(format!(
                    "VM device source must be below /dev: {}",
                    device.source().display()
                ))
            })?;
            let source = Path::new(HOST_DEV_MOUNT).join(relative_source);
            self.run_in_pod_root(
                paths,
                pid,
                "link pod device",
                &[
                    OsString::from("device-link"),
                    OsString::from("--path"),
                    device.path().as_os_str().to_owned(),
                    OsString::from("--source"),
                    source.as_os_str().to_owned(),
                ],
            )?;
        }
        Ok(())
    }

    fn check_static_sources(&self, pod: &PodId, storage: &PodMounts) -> Result<(), RuntimeError> {
        for path in [
            storage.root(),
            storage.workspace(),
            storage.docker(),
            storage.temporary(),
        ] {
            require_real_directory(path)?;
        }
        require_real_directory(&self.config.nix_store)?;
        if self.config.policy.nix_daemon() {
            require_real_directory(&self.config.pod_nix_store)?;
            require_real_directory(&self.config.nix_daemon_socket_directory)?;
            require_real_directory(&self.config.nix_gc_root_path(pod))?;
        }
        for share in &self.config.shares {
            require_real_directory(&share.source)?;
        }
        for program in [
            &self.config.podd_program,
            &self.config.podctl_program,
            &self.config.shell_program,
            &self.config.terminal_shell_program,
            &self.config.dockerd_program,
            &self.config.docker_program,
            &self.config.podman_program,
            &self.config.newuidmap_program,
            &self.config.newgidmap_program,
            &self.config.nix_program,
        ] {
            canonical_store_executable(program, &self.config.nix_store)?;
        }
        Ok(())
    }

    fn resolve_shares(
        &self,
        image_config: &ImageConfig,
        paths: &PodRuntimePaths,
    ) -> Result<Vec<ResolvedPodShare>, RuntimeError> {
        let home = image_home(image_config)?;
        let mut destinations = Vec::with_capacity(self.config.shares.len());
        for share in &self.config.shares {
            let destination = resolve_share_path(&share.path, &home)?;
            validate_share_destination(&destination, share.runtime_origin)?;
            if destinations.iter().any(|existing: &PathBuf| {
                existing == &destination
                    || existing.starts_with(&destination)
                    || destination.starts_with(existing)
            }) {
                return Err(RuntimeError::InvalidConfig(format!(
                    "workspace share destination overlaps another share: {}",
                    destination.display()
                )));
            }
            destinations.push(destination.clone());
        }
        Ok(self
            .config
            .shares
            .iter()
            .zip(destinations)
            .map(|(share, destination)| ResolvedPodShare {
                source: share.source.clone(),
                mountpoint: paths.share(&share.name),
                destination,
                home_relative: share.path.starts_with('~'),
                read_only: share.read_only,
                runtime_origin: share.runtime_origin,
                recursive_bind: share.recursive_bind,
            })
            .collect())
    }

    fn require_prepared(&self, pod: &PodId) -> Result<(), RuntimeError> {
        let path = self.config.runtime_root.join(pod.as_str());
        match path_state(&path)? {
            Some(metadata) if metadata.is_dir() => Ok(()),
            Some(_) => Err(RuntimeError::UnsafePath(path)),
            None => Err(RuntimeError::NotPrepared(pod.clone())),
        }
    }

    fn create_namespaces(
        &self,
        user_target: &Path,
        mount_target: &Path,
    ) -> Result<(), RuntimeError> {
        let mut user_namespace = OsString::from("--user=");
        user_namespace.push(user_target.as_os_str());
        let mut mount_namespace = OsString::from("--mount=");
        mount_namespace.push(mount_target.as_os_str());
        let arguments = vec![
            user_namespace,
            mount_namespace,
            OsString::from(format!(
                "--map-users=0:{}:{POD_ID_MAP_SIZE}",
                self.config.uid_base
            )),
            OsString::from(format!(
                "--map-groups=0:{}:{POD_ID_MAP_SIZE}",
                self.config.gid_base
            )),
            OsString::from("--propagation"),
            OsString::from("private"),
            OsString::from("--"),
            OsString::from(TRUE_PROGRAM),
        ];
        self.run(
            "create persistent pod user and mount namespaces",
            &self.config.unshare_program,
            &arguments,
        )?;
        Ok(())
    }

    fn expose_user_namespace(
        &self,
        user_namespace: &Path,
        mount_namespace: &Path,
    ) -> Result<(), RuntimeError> {
        let initial_user_namespace = initial_mount_namespace_path(user_namespace)?;
        let mut namespace = OsString::from("--mount=");
        namespace.push(mount_namespace.as_os_str());

        // The persistent pins are deliberately placed on a private PID 1
        // tmpfs after this pod mount namespace was cloned. Seed the userns
        // handle into the child through procfs's cross-namespace root magic
        // link so libmount can open a real user namespace for ID mapping.
        // Disabling canonicalization is essential: resolving the magic link
        // back to its ordinary pathname would select the child's empty file.
        self.run(
            "expose pod user namespace inside its mount namespace",
            &self.config.nsenter_program,
            &[
                namespace.clone(),
                OsString::from("--"),
                self.config.mount_program.as_os_str().to_owned(),
                OsString::from("--no-canonicalize"),
                OsString::from("--bind"),
                OsString::from("--"),
                initial_user_namespace.as_os_str().to_owned(),
                user_namespace.as_os_str().to_owned(),
            ],
        )?;
        self.run(
            "make child-visible user namespace bind private",
            &self.config.nsenter_program,
            &[
                namespace,
                OsString::from("--"),
                self.config.mount_program.as_os_str().to_owned(),
                OsString::from("--make-private"),
                OsString::from("--"),
                user_namespace.as_os_str().to_owned(),
            ],
        )?;
        Ok(())
    }

    fn mount_idmapped(
        &self,
        source: &Path,
        target: &Path,
        user_namespace: &Path,
        mount_namespace: &Path,
    ) -> Result<(), RuntimeError> {
        let mut namespace = OsString::from("--mount=");
        namespace.push(mount_namespace.as_os_str());
        let arguments = vec![
            namespace.clone(),
            OsString::from("--"),
            self.config.mount_program.as_os_str().to_owned(),
            OsString::from("--bind"),
            OsString::from("--map-users"),
            user_namespace.as_os_str().to_owned(),
            OsString::from("--"),
            source.as_os_str().to_owned(),
            target.as_os_str().to_owned(),
        ];
        self.run(
            "create namespace-private idmapped bind mount",
            &self.config.nsenter_program,
            &arguments,
        )?;
        // Create and privatize the mount after the pod-owned mount namespace
        // exists. Runc joins this exact user/mount namespace pair, so the
        // kernel never has to lock an inherited mount while crossing the user
        // namespace boundary.
        self.run(
            "make namespace-private idmapped bind mount private",
            &self.config.nsenter_program,
            &[
                namespace,
                OsString::from("--"),
                self.config.mount_program.as_os_str().to_owned(),
                OsString::from("--make-private"),
                OsString::from("--"),
                target.as_os_str().to_owned(),
            ],
        )?;
        Ok(())
    }

    fn mount_bind(
        &self,
        source: &Path,
        target: &Path,
        mount_namespace: &Path,
        recursive: bool,
    ) -> Result<(), RuntimeError> {
        let (bind, private) = if recursive {
            ("--rbind", "--make-rprivate")
        } else {
            ("--bind", "--make-private")
        };
        let mut namespace = OsString::from("--mount=");
        namespace.push(mount_namespace.as_os_str());
        self.run(
            "create namespace-private bind mount",
            &self.config.nsenter_program,
            &[
                namespace.clone(),
                OsString::from("--"),
                self.config.mount_program.as_os_str().to_owned(),
                OsString::from(bind),
                OsString::from("--"),
                source.as_os_str().to_owned(),
                target.as_os_str().to_owned(),
            ],
        )?;
        self.run(
            "make namespace-private bind mount private",
            &self.config.nsenter_program,
            &[
                namespace,
                OsString::from("--"),
                self.config.mount_program.as_os_str().to_owned(),
                OsString::from(private),
                OsString::from("--"),
                target.as_os_str().to_owned(),
            ],
        )?;
        Ok(())
    }

    fn unmount(&self, target: &Path) -> Result<(), RuntimeError> {
        let result = self.run(
            "unmount pod storage",
            &self.config.umount_program,
            &[OsString::from("--"), target.as_os_str().to_owned()],
        );
        match result {
            Ok(_) => Ok(()),
            // An interrupted cleanup can leave only a suffix of the two
            // namespace pins behind. util-linux reports an already-unmounted target
            // as an error; the kernel mount table is the authoritative check
            // which lets a retry continue without weakening live-mount safety.
            Err(_) if !mountinfo_contains(target)? => Ok(()),
            Err(error) => Err(error),
        }
    }

    fn runc_contains(&self, pod: &PodId) -> Result<bool, RuntimeError> {
        let output = self.run_runc_output(
            "list pods after failed deletion",
            [OsString::from("list"), OsString::from("--format=json")],
        )?;
        let entries: Option<Vec<RuncListEntry>> =
            serde_json::from_slice(&output.stdout).map_err(|source| RuntimeError::Json {
                path: PathBuf::from("runc-list"),
                source,
            })?;
        Ok(entries
            .unwrap_or_default()
            .iter()
            .any(|entry| entry.id == pod.as_str()))
    }

    fn rollback_create(
        &self,
        pod: &PodId,
        paths: &PodRuntimePaths,
        runc_attempted: bool,
        mounted: usize,
    ) -> Result<(), RuntimeError> {
        if runc_attempted {
            let deletion = self.run_runc(
                "rollback runc create",
                [
                    OsString::from("delete"),
                    OsString::from("--force"),
                    pod.as_str().into(),
                ],
            );
            let listed = self.runc_contains(pod);
            match (deletion, listed) {
                // The complete runc listing is authoritative even when a
                // previous force-delete returned an error after doing its
                // work. Only confirmed absence permits namespace cleanup.
                (_, Ok(false)) => {}
                (Err(error), Ok(true)) => return Err(error),
                (Ok(()), Ok(true)) => {
                    return Err(RuntimeError::InvalidState {
                        pod: pod.clone(),
                        reason: "runc still listed the pod after forced rollback".to_owned(),
                    });
                }
                (Ok(()), Err(check)) => return Err(check),
                (Err(error), Err(check)) => {
                    return Err(RuntimeError::RollbackFailed {
                        operation: "verify failed runc create rollback",
                        cause: error.to_string(),
                        rollback: check.to_string(),
                    });
                }
            }
        }
        self.cleanup_local_mounts(paths, mounted)
    }

    fn cleanup_local(&self, paths: &PodRuntimePaths) -> Result<(), RuntimeError> {
        self.cleanup_local_mounts(paths, paths.cleanup_mountpoints().len())
    }

    fn cleanup_local_mounts(
        &self,
        paths: &PodRuntimePaths,
        mounted: usize,
    ) -> Result<(), RuntimeError> {
        unmount_in_reverse(&paths.cleanup_mountpoints()[..mounted], |mountpoint| {
            self.unmount(mountpoint)
        })?;
        remove_directory_tree(&paths.pod)
    }

    fn state_locked(&self, pod: &PodId) -> Result<ContainerState, RuntimeError> {
        let output = self.run_runc_output(
            "inspect pod state",
            [OsString::from("state"), pod.as_str().into()],
        )?;
        let raw: RuncState =
            serde_json::from_slice(&output.stdout).map_err(|source| RuntimeError::Json {
                path: PathBuf::from(format!("runc-state:{pod}")),
                source,
            })?;
        if raw.id != pod.as_str() {
            return Err(RuntimeError::InvalidState {
                pod: pod.clone(),
                reason: format!("runc returned ID {:?}", raw.id),
            });
        }
        let expected_bundle = PodRuntimePaths::new(&self.config.runtime_root, pod).bundle;
        if raw
            .bundle
            .as_ref()
            .is_some_and(|bundle| bundle != &expected_bundle)
        {
            return Err(RuntimeError::InvalidState {
                pod: pod.clone(),
                reason: format!("runc returned unexpected bundle path {:?}", raw.bundle),
            });
        }
        let pid = raw.pid.filter(|pid| *pid != 0);
        if pid.is_some_and(|pid| pid > i32::MAX.cast_unsigned()) {
            return Err(RuntimeError::InvalidState {
                pod: pod.clone(),
                reason: "init PID does not fit Linux pid_t".to_owned(),
            });
        }
        if pid.is_none()
            && matches!(
                raw.status.as_str(),
                "creating" | "created" | "running" | "paused"
            )
        {
            return Err(RuntimeError::InvalidState {
                pod: pod.clone(),
                reason: "live lifecycle status did not include a PID".to_owned(),
            });
        }
        let status = match raw.status.as_str() {
            "creating" => ContainerStatus::Creating,
            "created" => ContainerStatus::Created,
            "running" => ContainerStatus::Running,
            "stopped" => ContainerStatus::Stopped,
            "paused" => ContainerStatus::Paused,
            _ => ContainerStatus::Unknown(raw.status),
        };
        Ok(ContainerState { status, pid })
    }

    #[expect(
        clippy::too_many_lines,
        reason = "the complete OCI security policy is intentionally reviewable in one place"
    )]
    fn oci_configuration(
        &self,
        pod: &PodId,
        image_config: &ImageConfig,
        paths: &PodRuntimePaths,
        shares: &[ResolvedPodShare],
        readiness_handshake: &str,
    ) -> Result<Value, RuntimeError> {
        let path = |value: &Path| {
            value.to_str().map(str::to_owned).ok_or_else(|| {
                RuntimeError::InvalidConfig(format!(
                    "OCI path is not valid UTF-8: {}",
                    value.display()
                ))
            })
        };
        let rootfs = path(&paths.rootfs)?;
        let workspace = path(&paths.workspace)?;
        let docker = path(&paths.docker)?;
        let temporary = path(&paths.temporary)?;
        let resolv_conf = path(&paths.resolv_conf)?;
        let hosts = path(&paths.hosts)?;
        let nix_store = path(if self.config.policy.nix_daemon() {
            &self.config.pod_nix_store
        } else {
            &self.config.nix_store
        })?;
        let podd_program =
            canonical_store_executable(&self.config.podd_program, &self.config.nix_store)?;
        let podctl_program =
            canonical_store_executable(&self.config.podctl_program, &self.config.nix_store)?;
        let tasci_program =
            canonical_store_executable(&self.config.tasci_program, &self.config.nix_store)?;
        let shell_program =
            canonical_store_executable(&self.config.shell_program, &self.config.nix_store)?;
        let dockerd_program =
            canonical_store_executable(&self.config.dockerd_program, &self.config.nix_store)?;
        let docker_client_program =
            canonical_store_executable(&self.config.docker_program, &self.config.nix_store)?;
        let podman_program =
            canonical_store_executable(&self.config.podman_program, &self.config.nix_store)?;
        let user_mapping_program =
            canonical_store_executable(&self.config.newuidmap_program, &self.config.nix_store)?;
        let group_mapping_program =
            canonical_store_executable(&self.config.newgidmap_program, &self.config.nix_store)?;
        let nix_client_program =
            canonical_store_executable(&self.config.nix_program, &self.config.nix_store)?;
        let podd = path(&podd_program)?;
        let podctl = path(&podctl_program)?;
        let tasci = path(&tasci_program)?;
        let shell = path(&shell_program)?;
        let dockerd = path(&dockerd_program)?;
        let docker_client = path(&docker_client_program)?;
        let podman = path(&podman_program)?;
        let user_mapping_helper = path(&user_mapping_program)?;
        let group_mapping_helper = path(&group_mapping_program)?;
        let nix_client = path(&nix_client_program)?;
        let nix_gc_root_source = path(&self.config.nix_gc_root_path(pod))?;
        let nix_gc_root_destination = path(&self.config.pod_nix_gc_root_path(pod))?;
        let user_namespace = path(&paths.user_namespace)?;
        let mount_namespace = path(&paths.mount_namespace)?;
        let readiness_directory = path(&paths.readiness)?;
        let nested_containers = self.config.policy.nested_containers();
        let capabilities = if nested_containers {
            DOCKER_PODD_CAPABILITIES
        } else {
            STANDARD_PODD_CAPABILITIES
        };
        debug_assert!(
            FORBIDDEN_CAPABILITIES
                .iter()
                .all(|forbidden| !capabilities.contains(forbidden))
        );

        let mut podd_arguments = vec![json!(podd)];
        podd_arguments.extend([
            Value::String("--ready-socket".to_owned()),
            Value::String(format!(
                "{READINESS_SOCKET_DESTINATION}/{READINESS_SOCKET_FILE}"
            )),
            Value::String("--ready-handshake".to_owned()),
            Value::String(readiness_handshake.to_owned()),
        ]);
        if nested_containers {
            podd_arguments.push(json!("--nested-containers"));
        }
        if self.config.policy.docker_daemon() {
            podd_arguments.extend([json!("--start-docker"), json!("--dockerd"), json!(dockerd)]);
        }
        if self.config.policy.rootless_containers() {
            let user = image_config.user();
            podd_arguments.extend([
                json!("--rootless-uid"),
                json!(user.uid().to_string()),
                json!("--rootless-gid"),
                json!(user.gid().to_string()),
            ]);
        }
        let user = image_config.user();
        podd_arguments.extend([
            json!("--init-directory"),
            json!("/run/tascarrel/hooks/init"),
            json!("--init-shell"),
            json!(shell),
            json!("--init-uid"),
            json!(user.uid().to_string()),
            json!("--init-gid"),
            json!(user.gid().to_string()),
        ]);
        for gid in image_config.user().additional_gids() {
            podd_arguments.extend([json!("--init-additional-gid"), json!(gid.to_string())]);
        }
        let mut devices = common_devices();
        if nested_containers || self.config.policy.rootless_containers() {
            devices.push(device("/dev/fuse", 10, 229));
        }
        if self.config.policy.rootless_containers() {
            devices.push(device("/dev/net/tun", 10, 200));
        }
        if self.config.policy.virtualization() {
            devices.push(device("/dev/kvm", KVM_DEVICE_MAJOR, KVM_DEVICE_MINOR));
        }
        // Workspace USB nodes come from the curated host-dev mount and are
        // linked into the private /dev after runc create. Listing their stable
        // aliases here makes rootless runc try to bind the same, nonexistent
        // alias path from the VM's /dev while creating the container.
        let device_rules = device_resource_rules(self.config.policy);

        let sysfs_options = if nested_containers {
            vec!["nosuid", "noexec", "nodev", "rw"]
        } else {
            vec!["nosuid", "noexec", "nodev", "ro"]
        };
        let cgroup_options = if nested_containers || self.config.policy.rootless_containers() {
            vec!["nosuid", "noexec", "nodev", "relatime", "rw"]
        } else {
            vec!["nosuid", "noexec", "nodev", "relatime", "ro"]
        };
        // Linux rejects a procfs mount from a nested user namespace when the
        // caller's current procfs is partially hidden by child mounts
        // (`mount_too_revealing`). Rootless Podman must therefore see a clean
        // outer procfs/sysfs mount. It has only set-ID capabilities in the
        // outer user namespace; sensitive host interfaces remain protected by
        // their owning namespaces and the pod's AppArmor profile.
        let mut readonly_paths = if self.config.policy.rootless_containers() {
            Vec::new()
        } else {
            vec![
                "/proc/asound",
                "/proc/bus",
                "/proc/fs",
                "/proc/irq",
                "/proc/sysrq-trigger",
            ]
        };
        // The Docker daemon configures bridge sysctls in the pod's private network
        // namespace. The global sysrq control remains separately read-only.
        if !nested_containers && !self.config.policy.rootless_containers() {
            readonly_paths.push("/proc/sys");
        }
        let process_environment =
            pod_process_environment(pod, image_config, self.config.policy.nix_daemon());
        let masked_paths = if self.config.policy.rootless_containers() {
            Vec::new()
        } else {
            vec![
                "/proc/acpi",
                "/proc/kcore",
                "/proc/keys",
                "/proc/latency_stats",
                "/proc/timer_list",
                "/proc/timer_stats",
                "/proc/sched_debug",
                "/proc/scsi",
                "/sys/firmware",
            ]
        };
        let apparmor_profile = self.config.policy.apparmor_profile();

        let mut configuration = json!({
            "ociVersion": "1.2.0",
            "hostname": pod.as_str(),
            "root": {
                "path": rootfs,
                "readonly": false
            },
            "process": {
                "terminal": false,
                "user": { "uid": 0, "gid": 0 },
                "args": podd_arguments,
                "env": process_environment,
                "cwd": "/workspace",
                "noNewPrivileges": !nested_containers,
                "apparmorProfile": apparmor_profile,
                "capabilities": {
                    "bounding": capabilities,
                    "effective": capabilities,
                    "inheritable": capabilities,
                    "permitted": capabilities,
                    "ambient": []
                },
                "rlimits": [{
                    "type": "RLIMIT_NOFILE",
                    "hard": if nested_containers { 1_048_576 } else { 65_536 },
                    "soft": if nested_containers { 1_048_576 } else { 65_536 }
                }]
            },
            "mounts": [
                {
                    "destination": "/proc",
                    "type": "proc",
                    "source": "proc",
                    "options": ["nosuid", "noexec", "nodev"]
                },
                {
                    "destination": "/dev",
                    "type": "tmpfs",
                    "source": "tmpfs",
                    "options": ["nosuid", "strictatime", "mode=755", "size=65536k"]
                },
                {
                    "destination": "/dev/pts",
                    "type": "devpts",
                    "source": "devpts",
                    "options": ["nosuid", "noexec", "newinstance", "ptmxmode=0666", "mode=0620", "gid=5"]
                },
                {
                    "destination": "/dev/shm",
                    "type": "tmpfs",
                    "source": "shm",
                    "options": ["nosuid", "noexec", "nodev", "mode=1777", "size=65536k"]
                },
                {
                    "destination": "/dev/mqueue",
                    "type": "mqueue",
                    "source": "mqueue",
                    "options": ["nosuid", "noexec", "nodev"]
                },
                {
                    "destination": "/run",
                    "type": "tmpfs",
                    "source": "tmpfs",
                    "options": ["nosuid", "nodev", "mode=755", "size=65536k"]
                },
                {
                    "destination": HOST_DEV_MOUNT,
                    "type": "none",
                    "source": POD_DEVICE_SOURCE_ROOT,
                    "options": ["rbind", "ro", "rprivate", "nosuid", "noexec"]
                },
                {
                    "destination": READINESS_SOCKET_DESTINATION,
                    "type": "none",
                    "source": readiness_directory,
                    "options": ["bind", "ro", "rprivate", "nosuid", "nodev", "noexec"]
                },
                {
                    "destination": "/tmp",
                    "type": "none",
                    "source": temporary,
                    "options": ["rbind", "rw", "rprivate", "nosuid", "nodev"]
                },
                {
                    "destination": "/sys",
                    "type": "sysfs",
                    "source": "sysfs",
                    "options": sysfs_options
                },
                {
                    "destination": "/sys/fs/cgroup",
                    "type": "cgroup",
                    "source": "cgroup",
                    "options": cgroup_options
                },
                {
                    "destination": "/nix/store",
                    "type": "none",
                    "source": nix_store,
                    "options": ["rbind", "ro", "rprivate", "nosuid", "nodev"]
                },
                {
                    "destination": "/etc/resolv.conf",
                    "type": "none",
                    "source": resolv_conf,
                    "options": ["bind", "ro", "rprivate", "nosuid", "nodev"]
                },
                {
                    "destination": "/etc/hosts",
                    "type": "none",
                    "source": hosts,
                    "options": ["bind", "ro", "rprivate", "nosuid", "nodev"]
                },
                {
                    "destination": "/workspace",
                    "type": "none",
                    "source": workspace,
                    "options": ["rbind", "rw", "rprivate", "nosuid", "nodev"]
                },
                {
                    "destination": "/var/lib/docker",
                    "type": "none",
                    "source": docker,
                    "options": ["rbind", "rw", "rprivate", "nosuid", "nodev"]
                }
            ],
            "linux": {
                "namespaces": [
                    { "type": "user", "path": user_namespace },
                    { "type": "mount", "path": mount_namespace },
                    { "type": "pid" },
                    { "type": "ipc" },
                    { "type": "uts" },
                    { "type": "cgroup" },
                    { "type": "network" }
                ],
                "cgroupsPath": self.cgroup_path(pod),
                "rootfsPropagation": "private",
                "devices": devices,
                "resources": {
                    "devices": device_rules
                },
                "seccomp": seccomp_profile(self.config.policy),
                "maskedPaths": masked_paths,
                "readonlyPaths": readonly_paths
            },
            "annotations": {
                "io.tascarrel.pod-id": pod.as_str(),
                "io.tascarrel.rootless-containers": self.config.policy.rootless_containers().to_string(),
                "io.tascarrel.nested-containers": nested_containers.to_string(),
                "io.tascarrel.virtualization": self.config.policy.virtualization().to_string(),
                "io.tascarrel.docker-daemon": self.config.policy.docker_daemon().to_string(),
                "io.tascarrel.podman": self.config.policy.podman().to_string(),
                "io.tascarrel.nix-daemon": self.config.policy.nix_daemon().to_string()
            }
        });
        let mounts = configuration["mounts"]
            .as_array_mut()
            .expect("OCI mounts are an array");
        for destination in [
            "/usr/local/bin/podctl",
            "/usr/local/bin/git-remote-tascarrel",
            "/usr/local/bin/tascarrel-git-receive-pack",
        ] {
            mounts.push(immutable_program_mount(destination, podctl.clone())?);
        }
        mounts.push(immutable_program_mount("/usr/local/bin/tasci-exec", tasci)?);
        if self.config.policy.rootless_containers() {
            let mounts = configuration["mounts"]
                .as_array_mut()
                .expect("OCI mounts are an array");
            for (source, destination) in [
                (&paths.subuid, "/etc/subuid"),
                (&paths.subgid, "/etc/subgid"),
            ] {
                mounts.push(json!({
                    "destination": destination,
                    "type": "none",
                    "source": path(source)?,
                    "options": ["bind", "ro", "rprivate", "nosuid", "nodev"]
                }));
            }
        }
        if self.config.policy.docker_daemon() {
            configuration["mounts"]
                .as_array_mut()
                .expect("OCI mounts are an array")
                .push(json!({
                    "destination": "/usr/local/bin/docker",
                    "type": "none",
                    "source": docker_client,
                    "options": ["bind", "ro", "rprivate", "nosuid", "nodev"]
                }));
        }
        if self.config.policy.podman() {
            let mounts = configuration["mounts"]
                .as_array_mut()
                .expect("OCI mounts are an array");
            for (source, destination) in [
                (podman.as_str(), PODMAN_PROGRAM_DESTINATION),
                (user_mapping_helper.as_str(), NEWUIDMAP_PROGRAM_DESTINATION),
                (group_mapping_helper.as_str(), NEWGIDMAP_PROGRAM_DESTINATION),
            ] {
                mounts.push(immutable_program_mount(destination, source.to_owned())?);
            }
            mounts.push(immutable_program_mount(
                CONTAINERS_POLICY_DESTINATION,
                path(&paths.containers_policy)?,
            )?);
        }
        if self.config.policy.nix_daemon() {
            let mounts = configuration["mounts"]
                .as_array_mut()
                .expect("OCI mounts are an array");
            mounts.push(json!({
                "destination": "/usr/local/bin/nix",
                "type": "none",
                "source": nix_client,
                "options": ["bind", "ro", "rprivate", "nosuid", "nodev"]
            }));
            mounts.push(json!({
                "destination": "/nix/var/nix/daemon-socket",
                "type": "none",
                "source": path(&self.config.nix_daemon_socket_directory)?,
                "options": ["rbind", "ro", "rprivate", "nosuid", "nodev"]
            }));
            mounts.push(json!({
                "destination": &nix_gc_root_destination,
                "type": "none",
                "source": &nix_gc_root_source,
                "options": ["rbind", "rw", "rprivate", "nosuid", "nodev"]
            }));
        }
        let mounts = configuration["mounts"]
            .as_array_mut()
            .expect("OCI mounts are an array");
        for share in shares {
            mounts.push(json!({
                "destination": path(&share.destination)?,
                "type": "none",
                "source": path(&share.mountpoint)?,
                "options": if share.read_only {
                    json!(["rbind", "ro", "rprivate", "nosuid", "nodev"])
                } else {
                    json!(["rbind", "rw", "rprivate", "nosuid", "nodev"])
                }
            }));
        }
        Ok(configuration)
    }

    fn runc_prefix(&self) -> Vec<OsString> {
        let mut arguments = vec![
            OsString::from("--root"),
            self.config.runc_root.as_os_str().to_owned(),
        ];
        if self.config.systemd_cgroup {
            arguments.push(OsString::from("--systemd-cgroup"));
        }
        arguments
    }

    fn cgroup_path(&self, pod: &PodId) -> String {
        if self.config.systemd_cgroup {
            format!(
                "system.slice:{}:{}",
                self.config.cgroup_parent,
                pod.as_str()
            )
        } else {
            format!("{}/{}", self.config.cgroup_parent, pod.as_str())
        }
    }

    fn run_runc<I>(&self, operation: &'static str, arguments: I) -> Result<(), RuntimeError>
    where
        I: IntoIterator<Item = OsString>,
    {
        self.run_runc_output(operation, arguments).map(|_| ())
    }

    fn run_runc_create(&self, pod: &PodId, paths: &PodRuntimePaths) -> Result<(), RuntimeError> {
        let mut arguments = self.runc_prefix();
        arguments.extend([
            OsString::from("--log"),
            paths.runc_create_log.as_os_str().to_owned(),
            OsString::from("--log-format"),
            OsString::from("json"),
            OsString::from("create"),
            OsString::from("--bundle"),
            paths.bundle.as_os_str().to_owned(),
            pod.as_str().into(),
        ]);
        let command = self.runner.run_detached_logged(
            &self.config.runc_program,
            &arguments,
            RUNC_CREATE_TIMEOUT,
            &paths.startup_log,
            STARTUP_LOG_LIMIT,
        );
        // runc opens this pre-created 0600 file with O_APPEND. Read only a
        // bounded diagnostic and remove it after the direct CLI child exits;
        // the container inherits guestd's journal descriptors, not this file.
        let log = read_bounded_file(&paths.runc_create_log, COMMAND_DIAGNOSTIC_LIMIT);
        let removal = fs::remove_file(&paths.runc_create_log)
            .map_err(|source| io_error("remove runc create log", &paths.runc_create_log, source));
        let mut output = command.map_err(|source| RuntimeError::CommandStart {
            operation: "create pod",
            program: self.config.runc_program.clone(),
            source,
        })?;
        let log = log?;
        removal?;
        if !log.is_empty() {
            output.stderr = log;
        }
        command_output("create pod", output).map(|_| ())
    }

    fn run_runc_output<I>(
        &self,
        operation: &'static str,
        arguments: I,
    ) -> Result<CommandOutput, RuntimeError>
    where
        I: IntoIterator<Item = OsString>,
    {
        let mut complete = self.runc_prefix();
        complete.extend(arguments);
        self.run(operation, &self.config.runc_program, &complete)
    }

    fn run(
        &self,
        operation: &'static str,
        program: &Path,
        arguments: &[OsString],
    ) -> Result<CommandOutput, RuntimeError> {
        let output =
            self.runner
                .run(program, arguments)
                .map_err(|source| RuntimeError::CommandStart {
                    operation,
                    program: program.to_path_buf(),
                    source,
                })?;
        command_output(operation, output)
    }

    fn lock(&self) -> Result<MutexGuard<'_, ()>, RuntimeError> {
        self.operation
            .lock()
            .map_err(|_| RuntimeError::LockPoisoned)
    }
}

#[derive(Debug)]
struct PodRuntimePaths {
    pod: PathBuf,
    bundle: PathBuf,
    mounts: PathBuf,
    user_namespace: PathBuf,
    mount_namespace: PathBuf,
    rootfs: PathBuf,
    workspace: PathBuf,
    docker: PathBuf,
    temporary: PathBuf,
    resolv_conf: PathBuf,
    hosts: PathBuf,
    subuid: PathBuf,
    subgid: PathBuf,
    containers_policy: PathBuf,
    runc_create_log: PathBuf,
    startup_log: PathBuf,
    usb_devices: PathBuf,
    readiness: PathBuf,
}

impl PodRuntimePaths {
    fn new(root: &Path, pod: &PodId) -> Self {
        let pod = root.join(pod.as_str());
        let bundle = pod.join(BUNDLE_DIRECTORY);
        let mounts = pod.join(MOUNTS_DIRECTORY);
        Self {
            user_namespace: bundle.join(USER_NAMESPACE_FILE),
            mount_namespace: bundle.join(MOUNT_NAMESPACE_FILE),
            rootfs: mounts.join(ROOTFS_MOUNT),
            workspace: mounts.join(WORKSPACE_MOUNT),
            docker: mounts.join(DOCKER_MOUNT),
            temporary: mounts.join(TEMPORARY_MOUNT),
            resolv_conf: bundle.join(RESOLV_CONF_FILE),
            hosts: bundle.join(HOSTS_FILE),
            subuid: bundle.join(SUBUID_FILE),
            subgid: bundle.join(SUBGID_FILE),
            containers_policy: bundle.join(CONTAINERS_POLICY_FILE),
            runc_create_log: bundle.join(RUNC_CREATE_LOG_FILE),
            startup_log: bundle.join(STARTUP_LOG_FILE),
            usb_devices: bundle.join(USB_DEVICES_FILE),
            readiness: bundle.join(READINESS_DIRECTORY),
            pod,
            bundle,
            mounts,
        }
    }

    fn mountpoints(&self) -> [&Path; 4] {
        [&self.rootfs, &self.workspace, &self.docker, &self.temporary]
    }

    fn cleanup_mountpoints(&self) -> [&Path; 2] {
        [&self.user_namespace, &self.mount_namespace]
    }

    fn share(&self, name: &str) -> PathBuf {
        self.mounts.join(format!("share-{name}"))
    }
}

/// Generates one fixed-size, versioned handshake with a fresh 128-bit nonce.
fn readiness_handshake() -> Result<String, RuntimeError> {
    let path = Path::new("/dev/urandom");
    let mut nonce = [0_u8; READINESS_NONCE_BYTES];
    File::open(path)
        .and_then(|mut source| source.read_exact(&mut nonce))
        .map_err(|source| io_error("read pod readiness nonce", path, source))?;
    let mut handshake = String::with_capacity(READINESS_HANDSHAKE_BYTES);
    handshake.push_str(READINESS_HANDSHAKE_PREFIX);
    for byte in nonce {
        use std::fmt::Write as _;
        write!(&mut handshake, "{byte:02x}").expect("writing to a String cannot fail");
    }
    debug_assert_eq!(handshake.len(), READINESS_HANDSHAKE_BYTES);
    Ok(handshake)
}

#[derive(Debug)]
#[allow(clippy::struct_excessive_bools)] // Each flag controls an independent mount property.
struct ResolvedPodShare {
    source: PathBuf,
    mountpoint: PathBuf,
    destination: PathBuf,
    home_relative: bool,
    read_only: bool,
    runtime_origin: bool,
    recursive_bind: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RuncState {
    id: String,
    status: String,
    #[serde(default)]
    pid: Option<u32>,
    #[serde(default)]
    bundle: Option<PathBuf>,
}

#[derive(Debug, Deserialize)]
struct RuncListEntry {
    id: String,
}

fn common_devices() -> Vec<Value> {
    vec![
        device("/dev/null", 1, 3),
        device("/dev/zero", 1, 5),
        device("/dev/full", 1, 7),
        device("/dev/random", 1, 8),
        device("/dev/urandom", 1, 9),
        device("/dev/tty", 5, 0),
    ]
}

fn device(path: &str, major: i64, minor: i64) -> Value {
    json!({
        "path": path,
        "type": "c",
        "major": major,
        "minor": minor,
        "fileMode": 0o666,
        "uid": 0,
        "gid": 0
    })
}

fn common_device_rules() -> Vec<Value> {
    let mut rules = vec![
        json!({ "allow": false, "access": "rwm" }),
        // `/dev` is a private tmpfs and the only external device source is
        // the guest-owned curated USB tree. Permit opening visible nodes, but
        // keep mknod denied unless an exact static rule below grants it. This
        // lets USB nodes be hotplugged without relying on `runc update`, which
        // deliberately skips device policy changes.
        json!({ "allow": true, "type": "a", "access": "rw" }),
    ];
    for (major, minor) in [
        (1, 3),
        (1, 5),
        (1, 7),
        (1, 8),
        (1, 9),
        (5, 0),
        // /dev/ptmx is a symlink to the pod's private devpts instance. It
        // still needs the kernel's 5:2 device permission, but no global
        // /dev/ptmx or /dev/console device node is exposed in the rootfs.
        (5, 2),
    ] {
        rules.push(device_rule(true, major, minor));
    }
    rules.push(json!({
        "allow": true,
        "type": "c",
        "major": 136,
        "access": "rwm"
    }));
    rules
}

fn device_rule(allow: bool, major: i64, minor: i64) -> Value {
    json!({
        "allow": allow,
        "type": "c",
        "major": major,
        "minor": minor,
        "access": "rwm"
    })
}

fn device_resource_rules(policy: PodPolicy) -> Vec<Value> {
    let mut rules = common_device_rules();
    if policy.nested_containers() || policy.rootless_containers() {
        rules.push(device_rule(true, 10, 229));
    }
    if policy.rootless_containers() {
        rules.push(device_rule(true, 10, 200));
    }
    if policy.virtualization() {
        rules.push(device_rule(true, KVM_DEVICE_MAJOR, KVM_DEVICE_MINOR));
    }
    rules
}

#[derive(Serialize)]
struct OciBindMount {
    destination: &'static str,
    #[serde(rename = "type")]
    mount_type: &'static str,
    source: String,
    options: [&'static str; 5],
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SeccompProfile {
    default_action: &'static str,
    syscalls: [SeccompRule; 1],
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SeccompRule {
    names: Vec<&'static str>,
    action: &'static str,
    errno_ret: i32,
}

/// Builds one fixed read-only bind mount for an injected immutable program.
fn immutable_program_mount(
    destination: &'static str,
    source: String,
) -> Result<Value, RuntimeError> {
    serde_json::to_value(OciBindMount {
        destination,
        mount_type: "none",
        source,
        options: ["bind", "ro", "rprivate", "nosuid", "nodev"],
    })
    .map_err(|source| RuntimeError::Json {
        path: PathBuf::from(destination),
        source,
    })
}

fn seccomp_profile(policy: PodPolicy) -> SeccompProfile {
    let mut blocked = GLOBAL_BLOCKED_SYSCALLS.to_vec();
    if !policy.nested_containers() && !policy.rootless_containers() {
        blocked.extend_from_slice(CONTAINER_BLOCKED_SYSCALLS);
    }
    SeccompProfile {
        default_action: "SCMP_ACT_ALLOW",
        syscalls: [SeccompRule {
            names: blocked,
            action: "SCMP_ACT_ERRNO",
            errno_ret: nix::libc::EPERM,
        }],
    }
}

fn validate_id_base(base: u32, kind: &str) -> Result<(), RuntimeError> {
    if base < ID_MAP_SIZE || base.checked_add(POD_ID_MAP_SIZE - 1).is_none() {
        return Err(RuntimeError::InvalidConfig(format!(
            "{kind} base must be at least {ID_MAP_SIZE} and leave room for {POD_ID_MAP_SIZE} IDs"
        )));
    }
    Ok(())
}

fn validate_absolute_path(path: &Path, purpose: &str) -> Result<(), RuntimeError> {
    if !path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::CurDir | Component::ParentDir | Component::Prefix(_)
            )
        })
    {
        return Err(RuntimeError::InvalidConfig(format!(
            "{purpose} must be an absolute normalized path: {}",
            path.display()
        )));
    }
    Ok(())
}

fn validate_share_path_expression(value: &str) -> Result<(), RuntimeError> {
    let path = if value == "~" {
        return Ok(());
    } else if let Some(relative) = value.strip_prefix("~/") {
        if relative.is_empty() {
            return Err(RuntimeError::InvalidConfig(
                "use `~` rather than `~/` for a share destination".to_owned(),
            ));
        }
        Path::new(relative)
    } else {
        if value.starts_with('~') {
            return Err(RuntimeError::InvalidConfig(
                "only `~` and `~/...` share home expansion are supported".to_owned(),
            ));
        }
        let path = Path::new(value);
        if !path.is_absolute() {
            return Err(RuntimeError::InvalidConfig(
                "workspace share destination must be absolute or begin with `~/`".to_owned(),
            ));
        }
        path
    };
    if path.components().any(|component| {
        matches!(
            component,
            Component::CurDir | Component::ParentDir | Component::Prefix(_)
        )
    }) {
        return Err(RuntimeError::InvalidConfig(
            "workspace share destination must be normalized".to_owned(),
        ));
    }
    Ok(())
}

fn image_home(config: &ImageConfig) -> Result<PathBuf, RuntimeError> {
    let home = config
        .environment()
        .iter()
        .filter_map(|entry| entry.split_once('='))
        .filter(|(name, _)| *name == "HOME")
        .map(|(_, value)| value)
        .next_back()
        .map_or_else(
            || {
                if config.user().uid() == 0 {
                    PathBuf::from("/root")
                } else {
                    PathBuf::from("/workspace")
                }
            },
            PathBuf::from,
        );
    validate_absolute_path(&home, "image user HOME")?;
    Ok(home)
}

fn resolve_share_path(value: &str, home: &Path) -> Result<PathBuf, RuntimeError> {
    validate_share_path_expression(value)?;
    if value == "~" {
        Ok(home.to_path_buf())
    } else if let Some(relative) = value.strip_prefix("~/") {
        Ok(home.join(relative))
    } else {
        Ok(PathBuf::from(value))
    }
}

fn validate_share_destination(path: &Path, runtime_origin: bool) -> Result<(), RuntimeError> {
    const RESERVED_EXACT: &[&str] = &[
        "/",
        "/proc",
        "/dev",
        "/sys",
        "/run",
        "/nix",
        "/workspace",
        "/var/lib/docker",
        "/bin/sh",
        "/etc/resolv.conf",
        "/usr/local/bin/docker",
        PODMAN_PROGRAM_DESTINATION,
        NEWUIDMAP_PROGRAM_DESTINATION,
        NEWGIDMAP_PROGRAM_DESTINATION,
        "/usr/local/bin/nix",
        "/usr/local/bin/podctl",
        "/usr/local/bin/tasci-exec",
        "/usr/local/bin/git-remote-tascarrel",
        "/usr/local/bin/tascarrel-git-receive-pack",
    ];
    validate_absolute_path(path, "resolved workspace share destination")?;
    let conflicts = RESERVED_EXACT.iter().any(|reserved| {
        let reserved = Path::new(reserved);
        path == reserved || reserved.starts_with(path)
    });
    let below_virtual_or_private = ["/proc", "/dev", "/sys", "/run", "/nix", "/var/lib/docker"]
        .iter()
        .any(|reserved| path.starts_with(reserved));
    let permitted_runtime_origin = runtime_origin
        && [
            "/run/tascarrel/https-ca",
            "/run/tascarrel/hooks",
            "/run/tascarrel/agents",
        ]
        .iter()
        .any(|destination| path == Path::new(destination));
    if (conflicts || below_virtual_or_private) && !permitted_runtime_origin {
        return Err(RuntimeError::InvalidConfig(format!(
            "workspace share destination conflicts with a runtime mount: {}",
            path.display()
        )));
    }
    Ok(())
}

fn prepare_share_destinations(
    storage: &PodMounts,
    image_config: &ImageConfig,
    shares: &[ResolvedPodShare],
) -> Result<(), RuntimeError> {
    for share in shares {
        let (root, relative, user_owned) = match share.destination.strip_prefix("/workspace") {
            Ok(relative) => (storage.workspace(), relative, true),
            Err(_) => (
                storage.root(),
                share
                    .destination
                    .strip_prefix("/")
                    .expect("resolved share destinations are absolute"),
                share.home_relative,
            ),
        };
        let (uid, gid) = if user_owned {
            (image_config.user().uid(), image_config.user().gid())
        } else {
            (0, 0)
        };
        create_owned_directory_tree(root, relative, uid, gid)?;
    }
    Ok(())
}

fn create_owned_directory_tree(
    root: &Path,
    relative: &Path,
    uid: u32,
    gid: u32,
) -> Result<(), RuntimeError> {
    require_real_directory(root)?;
    let mut path = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(component) = component else {
            return Err(RuntimeError::UnsafePath(root.join(relative)));
        };
        path.push(component);
        match path_state(&path)? {
            Some(metadata) if metadata.is_dir() => {}
            Some(_) => return Err(RuntimeError::UnsafePath(path)),
            None => {
                let mut builder = DirBuilder::new();
                builder.mode(0o755);
                builder
                    .create(&path)
                    .map_err(|source| io_error("create share destination", &path, source))?;
                set_directory_owner(&path, uid, gid)?;
            }
        }
    }
    Ok(())
}

fn set_directory_owner(path: &Path, uid: u32, gid: u32) -> Result<(), RuntimeError> {
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_CLOEXEC | nix::libc::O_DIRECTORY | nix::libc::O_NOFOLLOW)
        .open(path)
        .map_err(|source| io_error("open share destination", path, source))?;
    nix::unistd::fchown(
        &directory,
        Some(nix::unistd::Uid::from_raw(uid)),
        Some(nix::unistd::Gid::from_raw(gid)),
    )
    .map_err(|source| {
        io_error(
            "set share destination ownership",
            path,
            io::Error::from_raw_os_error(source as i32),
        )
    })
}

fn pod_process_environment(pod: &PodId, config: &ImageConfig, nix_daemon: bool) -> Vec<String> {
    let mut environment = BTreeMap::new();
    for entry in config.environment() {
        let (name, value) = entry
            .split_once('=')
            .expect("ImageConfig validates environment entries");
        environment.insert(name.to_owned(), value.to_owned());
    }
    // The image user applies to user-facing execs. The namespace init remains
    // root because it supervises podd and, for nested pods, dockerd.
    environment.insert("HOME".to_owned(), "/root".to_owned());
    environment.insert("USER".to_owned(), "root".to_owned());
    environment.insert("LOGNAME".to_owned(), "root".to_owned());
    environment.insert("TASCARREL_POD_ID".to_owned(), pod.as_str().to_owned());
    environment.entry("PATH".to_owned()).or_insert_with(|| {
        "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_owned()
    });
    environment
        .entry("TERM".to_owned())
        .or_insert_with(|| "xterm-256color".to_owned());
    if nix_daemon {
        // Runtime-owned so an image cannot silently disable the workspace's
        // supported Nix CLI surface for pod init or its descendants.
        environment.insert(
            "NIX_CONFIG".to_owned(),
            "experimental-features = nix-command flakes".to_owned(),
        );
    }
    environment
        .into_iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect()
}

fn path_state(path: &Path) -> Result<Option<fs::Metadata>, RuntimeError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Err(RuntimeError::UnsafePath(path.to_path_buf()))
        }
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(io_error("inspect", path, source)),
    }
}

fn require_real_directory(path: &Path) -> Result<(), RuntimeError> {
    match path_state(path)? {
        Some(metadata) if metadata.is_dir() => require_canonical_path(path),
        _ => Err(RuntimeError::UnsafePath(path.to_path_buf())),
    }
}

fn canonical_store_executable(path: &Path, store: &Path) -> Result<PathBuf, RuntimeError> {
    let canonical_store =
        fs::canonicalize(store).map_err(|source| io_error("canonicalize", store, source))?;
    let canonical =
        fs::canonicalize(path).map_err(|source| io_error("canonicalize", path, source))?;
    if canonical == canonical_store || !canonical.starts_with(&canonical_store) {
        return Err(RuntimeError::UnsafePath(path.to_path_buf()));
    }
    let metadata = fs::symlink_metadata(&canonical)
        .map_err(|source| io_error("inspect", &canonical, source))?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.permissions().mode() & 0o111 == 0
    {
        return Err(RuntimeError::UnsafePath(path.to_path_buf()));
    }
    Ok(canonical)
}

fn require_canonical_path(path: &Path) -> Result<(), RuntimeError> {
    let canonical =
        fs::canonicalize(path).map_err(|source| io_error("canonicalize", path, source))?;
    if canonical == path {
        Ok(())
    } else {
        Err(RuntimeError::UnsafePath(path.to_path_buf()))
    }
}

fn ensure_private_directory(path: &Path) -> Result<(), RuntimeError> {
    ensure_directory(path, 0o700)
}

fn ensure_searchable_directory(path: &Path) -> Result<(), RuntimeError> {
    ensure_directory(path, 0o711)
}

fn ensure_directory(path: &Path, mode: u32) -> Result<(), RuntimeError> {
    validate_absolute_path(path, "runtime directory")?;
    match path_state(path)? {
        Some(metadata) if metadata.is_dir() => {
            fs::set_permissions(path, fs::Permissions::from_mode(mode))
                .map_err(|source| io_error("secure directory", path, source))?;
            Ok(())
        }
        Some(_) => Err(RuntimeError::UnsafePath(path.to_path_buf())),
        None => {
            fs::create_dir_all(path)
                .map_err(|source| io_error("create directory", path, source))?;
            fs::set_permissions(path, fs::Permissions::from_mode(mode))
                .map_err(|source| io_error("secure directory", path, source))
        }
    }
}

fn create_private_directory(path: &Path) -> Result<(), RuntimeError> {
    create_directory(path, 0o700)
}

fn create_searchable_directory(path: &Path) -> Result<(), RuntimeError> {
    create_directory(path, 0o711)
}

fn create_directory(path: &Path, mode: u32) -> Result<(), RuntimeError> {
    if path_state(path)?.is_some() {
        return Err(RuntimeError::UnsafePath(path.to_path_buf()));
    }
    let mut builder = DirBuilder::new();
    builder.mode(mode);
    builder
        .create(path)
        .map_err(|source| io_error("create directory", path, source))
}

fn remove_directory_tree(path: &Path) -> Result<(), RuntimeError> {
    match path_state(path)? {
        None => Ok(()),
        Some(metadata) if metadata.is_dir() => {
            fs::remove_dir_all(path).map_err(|source| io_error("remove directory", path, source))
        }
        Some(_) => Err(RuntimeError::UnsafePath(path.to_path_buf())),
    }
}

fn initial_mount_namespace_path(path: &Path) -> Result<PathBuf, RuntimeError> {
    let relative = path.strip_prefix(Path::new("/")).map_err(|_| {
        RuntimeError::InvalidConfig(format!(
            "initial-namespace path must be absolute: {}",
            path.display()
        ))
    })?;
    Ok(Path::new(INITIAL_MOUNT_NAMESPACE_ROOT).join(relative))
}

fn unmount_in_reverse<E>(
    mountpoints: &[&Path],
    mut unmount: impl FnMut(&Path) -> Result<(), E>,
) -> Result<(), E> {
    for mountpoint in mountpoints.iter().rev() {
        // Stop immediately when the mount namespace pin cannot be released.
        // The user namespace and local tree must remain available for a safe
        // retry instead of being partially dismantled.
        unmount(mountpoint)?;
    }
    Ok(())
}

fn mountinfo_contains(target: &Path) -> Result<bool, RuntimeError> {
    let mountinfo_path = Path::new(MOUNTINFO);
    let contents = fs::read(mountinfo_path)
        .map_err(|source| io_error("read mount table", mountinfo_path, source))?;
    for line in contents.split(|byte| *byte == b'\n') {
        let Some(encoded) = line.split(|byte| *byte == b' ').nth(4) else {
            continue;
        };
        let decoded = decode_mountinfo_path(encoded)
            .map_err(|source| io_error("decode mount table", mountinfo_path, source))?;
        if OsString::from_vec(decoded).as_os_str() == target.as_os_str() {
            return Ok(true);
        }
    }
    Ok(false)
}

fn decode_mountinfo_path(encoded: &[u8]) -> io::Result<Vec<u8>> {
    let mut decoded = Vec::with_capacity(encoded.len());
    let mut index = 0;
    while index < encoded.len() {
        if encoded[index] != b'\\' {
            decoded.push(encoded[index]);
            index += 1;
            continue;
        }
        let octal = encoded.get(index + 1..index + 4).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "truncated mountinfo escape")
        })?;
        if !octal.iter().all(u8::is_ascii_digit) || octal.iter().any(|byte| *byte > b'7') {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid mountinfo escape",
            ));
        }
        decoded.push((octal[0] - b'0') * 64 + (octal[1] - b'0') * 8 + (octal[2] - b'0'));
        index += 4;
    }
    Ok(decoded)
}

fn write_json_exclusive(path: &Path, value: &Value) -> Result<(), RuntimeError> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|source| io_error("create OCI configuration", path, source))?;
    serde_json::to_writer_pretty(&mut file, value).map_err(|source| RuntimeError::Json {
        path: path.to_path_buf(),
        source,
    })?;
    file.write_all(b"\n")
        .map_err(|source| io_error("write OCI configuration", path, source))?;
    file.sync_all()
        .map_err(|source| io_error("sync OCI configuration", path, source))?;
    File::open(path.parent().unwrap_or(Path::new("/")))
        .and_then(|directory| directory.sync_all())
        .map_err(|source| io_error("sync bundle directory", path, source))
}

fn write_json_replace(path: &Path, value: &Value) -> Result<(), RuntimeError> {
    let mut file = OpenOptions::new()
        .write(true)
        .truncate(true)
        .custom_flags(nix::libc::O_CLOEXEC | nix::libc::O_NOFOLLOW)
        .open(path)
        .map_err(|source| io_error("open runtime JSON", path, source))?;
    if !file
        .metadata()
        .map_err(|source| io_error("inspect runtime JSON", path, source))?
        .is_file()
    {
        return Err(RuntimeError::UnsafePath(path.to_owned()));
    }
    serde_json::to_writer_pretty(&mut file, value).map_err(|source| RuntimeError::Json {
        path: path.to_owned(),
        source,
    })?;
    file.write_all(b"\n")
        .map_err(|source| io_error("write runtime JSON", path, source))?;
    file.sync_all()
        .map_err(|source| io_error("sync runtime JSON", path, source))
}

fn read_device_manifest(path: &Path) -> Result<Vec<PodDevice>, RuntimeError> {
    const LIMIT: usize = 1024 * 1024;
    let bytes = read_bounded_file(path, LIMIT)?;
    serde_json::from_slice(&bytes).map_err(|source| RuntimeError::Json {
        path: path.to_owned(),
        source,
    })
}

fn write_file_exclusive(path: &Path, contents: &[u8], mode: u32) -> Result<(), RuntimeError> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(path)
        .map_err(|source| io_error("create runtime file", path, source))?;
    file.write_all(contents)
        .map_err(|source| io_error("write runtime file", path, source))?;
    file.sync_all()
        .map_err(|source| io_error("sync runtime file", path, source))
}

/// Builds the image user's full subordinate-ID delegation within the pod map.
fn subordinate_id_file(user: &ImageUser) -> String {
    format!("{}:{ID_MAP_SIZE}:{ID_MAP_SIZE}\n", user.name())
}

fn read_bounded_file(path: &Path, limit: usize) -> Result<Vec<u8>, RuntimeError> {
    let file = File::open(path).map_err(|source| io_error("open diagnostic", path, source))?;
    let mut contents = Vec::new();
    file.take(u64::try_from(limit).unwrap_or(u64::MAX))
        .read_to_end(&mut contents)
        .map_err(|source| io_error("read diagnostic", path, source))?;
    Ok(contents)
}

fn command_output(
    operation: &'static str,
    output: CommandOutput,
) -> Result<CommandOutput, RuntimeError> {
    if output.success {
        return Ok(output);
    }
    let detail = String::from_utf8_lossy(&output.stderr)
        .trim()
        .chars()
        .take(COMMAND_DIAGNOSTIC_LIMIT)
        .collect::<String>();
    Err(RuntimeError::CommandFailed {
        operation,
        detail: if detail.is_empty() {
            "command exited unsuccessfully".to_owned()
        } else {
            detail
        },
    })
}

fn io_error(operation: &'static str, path: &Path, source: io::Error) -> RuntimeError {
    RuntimeError::Io {
        operation,
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::fs::symlink;
    use std::sync::Mutex;

    use tempfile::TempDir;

    use super::super::ImageUser;
    use super::*;

    impl RuntimeConfig {
        fn runtime_root(&self) -> &Path {
            &self.runtime_root
        }

        fn runc_root(&self) -> &Path {
            &self.runc_root
        }
    }

    impl<R> PodRuntime<R> {
        const fn config(&self) -> &RuntimeConfig {
            &self.config
        }
    }

    impl<R: CommandRunner> PodRuntime<R> {
        fn create_from_mounts(
            &self,
            pod: &PodId,
            storage: &PodMounts,
        ) -> Result<CreatedPod, RuntimeError> {
            self.create_from_mounts_and_config(pod, storage, &ImageConfig::default())
        }
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    struct Invocation {
        program: PathBuf,
        arguments: Vec<OsString>,
        detached: bool,
    }

    #[derive(Clone, Default)]
    struct FakeRunner {
        state: Arc<FakeRunnerState>,
    }

    #[derive(Default)]
    struct FakeRunnerState {
        invocations: Mutex<Vec<Invocation>>,
        failures: Mutex<VecDeque<String>>,
        state: Mutex<Option<Vec<u8>>>,
        list: Mutex<Vec<u8>>,
    }

    impl FakeRunner {
        fn with_created_state() -> Self {
            let runner = Self::default();
            *runner.state.state.lock().unwrap() = Some(
                br#"{"ociVersion":"1.2.0","id":"pod-1","status":"created","pid":4242}"#.to_vec(),
            );
            *runner.state.list.lock().unwrap() = br#"[{"id":"pod-1"}]"#.to_vec();
            runner
        }

        fn fail_once(&self, fragment: &str) {
            self.state
                .failures
                .lock()
                .unwrap()
                .push_back(fragment.to_owned());
        }

        fn invocations(&self) -> Vec<Invocation> {
            self.state.invocations.lock().unwrap().clone()
        }

        fn run_impl(
            &self,
            program: &Path,
            arguments: &[OsString],
            detached: bool,
        ) -> CommandOutput {
            self.state.invocations.lock().unwrap().push(Invocation {
                program: program.to_path_buf(),
                arguments: arguments.to_vec(),
                detached,
            });
            let rendered = arguments
                .iter()
                .map(|argument| argument.to_string_lossy())
                .collect::<Vec<_>>()
                .join(" ");
            let fail = self
                .state
                .failures
                .lock()
                .unwrap()
                .front()
                .is_some_and(|fragment| rendered.contains(fragment));
            if fail {
                self.state.failures.lock().unwrap().pop_front();
                return CommandOutput::failure("injected runtime failure");
            }
            if arguments.iter().any(|argument| argument == "state") {
                return CommandOutput {
                    success: true,
                    stdout: self.state.state.lock().unwrap().clone().unwrap_or_default(),
                    stderr: Vec::new(),
                };
            }
            if arguments.iter().any(|argument| argument == "list") {
                return CommandOutput {
                    success: true,
                    stdout: self.state.list.lock().unwrap().clone(),
                    stderr: Vec::new(),
                };
            }
            if arguments.iter().any(|argument| argument == "delete") {
                *self.state.list.lock().unwrap() = b"[]".to_vec();
                *self.state.state.lock().unwrap() = None;
            }
            CommandOutput::success()
        }
    }

    impl CommandRunner for FakeRunner {
        fn run(&self, program: &Path, arguments: &[OsString]) -> io::Result<CommandOutput> {
            Ok(self.run_impl(program, arguments, false))
        }

        fn run_detached(
            &self,
            program: &Path,
            arguments: &[OsString],
            _timeout: Duration,
        ) -> io::Result<CommandOutput> {
            Ok(self.run_impl(program, arguments, true))
        }
    }

    struct Fixture {
        temporary: TempDir,
        runtime: PodRuntime<FakeRunner>,
        runner: FakeRunner,
        mounts: PodMounts,
        pod: PodId,
    }

    impl Fixture {
        fn new() -> Self {
            Self::with_systemd_cgroup(false)
        }

        fn with_systemd_cgroup(systemd_cgroup: bool) -> Self {
            Self::with_policy(systemd_cgroup, PodPolicy::default())
        }

        fn with_options(systemd_cgroup: bool, rootless_containers: bool) -> Self {
            Self::with_policy(
                systemd_cgroup,
                PodPolicy::default().with_podman(rootless_containers),
            )
        }

        fn with_policy(systemd_cgroup: bool, policy: PodPolicy) -> Self {
            Self::with_policy_and_shares(systemd_cgroup, policy, &[])
        }

        fn with_policy_and_shares(
            systemd_cgroup: bool,
            policy: PodPolicy,
            shares: &[(&str, &str)],
        ) -> Self {
            Self::with_policy_shares_and_devices(systemd_cgroup, policy, shares, &[])
        }

        fn with_devices(devices: &[PodDevice]) -> Self {
            Self::with_policy_shares_and_devices(false, PodPolicy::default(), &[], devices)
        }

        #[allow(clippy::too_many_lines)] // The fixture constructs one complete runtime filesystem.
        fn with_policy_shares_and_devices(
            systemd_cgroup: bool,
            policy: PodPolicy,
            shares: &[(&str, &str)],
            devices: &[PodDevice],
        ) -> Self {
            let temporary = tempfile::tempdir().unwrap();
            let root = temporary.path();
            let runtime_root = root.join("runtime");
            let runc_root = root.join("runc");
            let nix_store = root.join("nix/store");
            let podd = nix_store.join("abcd-tascarrel-podd/bin/tascarrel-podd");
            let podctl = nix_store.join("bcde-tascarrel-podctl/bin/podctl");
            let tasci = nix_store.join("cdef-tasci-exec/bin/tasci-exec");
            let shell = nix_store.join("efgh-bash/bin/bash");
            let dockerd = nix_store.join("ijkl-docker/bin/dockerd");
            let docker_client = nix_store.join("mnop-docker-client/bin/docker");
            let podman = nix_store.join("opqr-podman/bin/podman");
            let user_mapping_helper = nix_store.join("stuv-shadow/bin/newuidmap");
            let group_mapping_helper = nix_store.join("stuv-shadow/bin/newgidmap");
            let nix_client = nix_store.join("qrst-nix/bin/nix");
            let pod_nix_store = root.join("persistent-nix/nix/store");
            let pod_nix_socket = root.join("run/pod-nix-daemon");
            let nix_gc_roots = root.join("persistent-nix/nix/var/nix/gcroots/tascarrel/pods");
            let pod_nix_gc_roots = PathBuf::from("/nix/var/nix/gcroots/tascarrel/pods");
            for program in [
                &podd,
                &podctl,
                &tasci,
                &shell,
                &dockerd,
                &docker_client,
                &podman,
                &user_mapping_helper,
                &group_mapping_helper,
                &nix_client,
            ] {
                fs::create_dir_all(program.parent().unwrap()).unwrap();
                fs::write(program, b"program").unwrap();
                fs::set_permissions(program, fs::Permissions::from_mode(0o555)).unwrap();
            }
            if policy.nix_daemon() {
                fs::create_dir_all(&pod_nix_store).unwrap();
                fs::create_dir_all(&pod_nix_socket).unwrap();
                fs::create_dir_all(nix_gc_roots.join("pod-1")).unwrap();
            }
            let storage = root.join("storage");
            let rootfs = storage.join("root");
            let workspace = storage.join("workspace");
            let docker = storage.join("docker");
            let pod_temporary = storage.join("temporary");
            for path in [&rootfs, &workspace, &docker, &pod_temporary] {
                fs::create_dir_all(path).unwrap();
            }
            let configured_shares = shares
                .iter()
                .map(|(name, path)| {
                    let source = root.join("shares").join(name);
                    fs::create_dir_all(&source).unwrap();
                    PodShare::new(*name, source, *path).unwrap()
                })
                .collect::<Vec<_>>();
            let config = RuntimeConfig::new(
                &runtime_root,
                &runc_root,
                PodPrograms::new(
                    &nix_store,
                    &podd,
                    &podctl,
                    &tasci,
                    &shell,
                    &shell,
                    &dockerd,
                    &docker_client,
                    &podman,
                    &user_mapping_helper,
                    &group_mapping_helper,
                    &nix_client,
                )
                .unwrap(),
                100_000,
                200_000,
            )
            .unwrap()
            .with_programs(
                "/tools/runc",
                "/tools/mount",
                "/tools/umount",
                "/tools/unshare",
                "/tools/nsenter",
                "/tools/ip",
            )
            .unwrap()
            .with_systemd_cgroup(systemd_cgroup)
            .with_policy(policy)
            .with_nix_service(
                pod_nix_store,
                pod_nix_socket,
                nix_gc_roots,
                pod_nix_gc_roots,
            )
            .unwrap()
            .with_shares(configured_shares)
            .and_then(|config| config.with_devices(devices.iter().cloned()))
            .unwrap();
            let runner = FakeRunner::with_created_state();
            let runtime = PodRuntime::with_runner(config, runner.clone()).unwrap();
            Self {
                temporary,
                runtime,
                runner,
                mounts: PodMounts::new(rootfs, workspace, docker, pod_temporary).unwrap(),
                pod: PodId::new("pod-1").unwrap(),
            }
        }

        fn create(&self) -> CreatedPod {
            self.runtime
                .create_from_mounts(&self.pod, &self.mounts)
                .unwrap()
        }

        fn create_with_config(&self, config: &ImageConfig) -> CreatedPod {
            self.runtime
                .create_from_mounts_and_config(&self.pod, &self.mounts, config)
                .unwrap()
        }

        fn configuration(&self) -> Value {
            let path = self
                .runtime
                .config()
                .runtime_root()
                .join(self.pod.as_str())
                .join(BUNDLE_DIRECTORY)
                .join(CONFIG_FILE);
            serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
        }
    }

    /// Verifies runtime tools are absolute store executables and ID maps
    /// exclude host root.
    #[test]
    fn configuration_rejects_relative_tools_and_host_root_mapping() {
        let temporary = tempfile::tempdir().unwrap();
        let store = temporary.path().join("nix/store");
        let podd = store.join("hash/bin/podd");
        let podctl = store.join("podctl/bin/podctl");
        let tasci = store.join("tasci/bin/tasci-exec");
        let shell = store.join("shell/bin/sh");
        let dockerd = store.join("docker/bin/dockerd");
        let docker_client = store.join("docker-client/bin/docker");
        let podman = store.join("podman/bin/podman");
        let user_mapping_helper = store.join("shadow/bin/newuidmap");
        let group_mapping_helper = store.join("shadow/bin/newgidmap");
        let nix_client = store.join("nix/bin/nix");
        let programs = PodPrograms::new(
            &store,
            &podd,
            &podctl,
            &tasci,
            &shell,
            &shell,
            &dockerd,
            &docker_client,
            &podman,
            &user_mapping_helper,
            &group_mapping_helper,
            &nix_client,
        )
        .unwrap();
        let config = RuntimeConfig::new(
            temporary.path().join("runtime"),
            temporary.path().join("runc"),
            programs,
            100_000,
            200_000,
        )
        .unwrap();
        assert!(
            config
                .clone()
                .with_programs("runc", "/m", "/u", "/unshare", "/n", "/ip")
                .is_err()
        );
        assert!(
            RuntimeConfig::new(
                "/runtime",
                "/runc",
                PodPrograms::new(
                    "/nix/store",
                    "/nix/store/x/podd",
                    "/nix/store/x/podctl",
                    "/nix/store/x/tasci-exec",
                    "/nix/store/x/sh",
                    "/nix/store/x/zsh",
                    "/nix/store/x/dockerd",
                    "/nix/store/x/docker",
                    "/nix/store/x/podman",
                    "/nix/store/x/newuidmap",
                    "/nix/store/x/newgidmap",
                    "/nix/store/x/nix"
                )
                .unwrap(),
                0,
                200_000
            )
            .is_err()
        );
        assert!(
            PodPrograms::new(
                "/nix/store",
                "/usr/bin/podd",
                "/nix/store/x/podctl",
                "/nix/store/x/tasci-exec",
                "/nix/store/x/sh",
                "/nix/store/x/zsh",
                "/nix/store/x/dockerd",
                "/nix/store/x/docker",
                "/nix/store/x/podman",
                "/nix/store/x/newuidmap",
                "/nix/store/x/newgidmap",
                "/nix/store/x/nix"
            )
            .is_err()
        );
    }

    /// Verifies injected program links resolve to immutable Nix store paths.
    #[test]
    fn immutable_program_symlinks_must_resolve_inside_the_nix_store() {
        let temporary = tempfile::tempdir().unwrap();
        let store = temporary.path().join("nix/store");
        let target = store.join("moby/bin/dockerd");
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(&target, b"program").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o555)).unwrap();
        let link = store.join("docker/bin/dockerd");
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        symlink(&target, &link).unwrap();
        assert_eq!(canonical_store_executable(&link, &store).unwrap(), target);

        let outside = temporary.path().join("outside");
        fs::write(&outside, b"program").unwrap();
        fs::set_permissions(&outside, fs::Permissions::from_mode(0o555)).unwrap();
        let escape = store.join("docker/bin/escape");
        symlink(outside, &escape).unwrap();
        assert!(canonical_store_executable(&escape, &store).is_err());
    }

    /// Verifies readiness handshakes have a stable protocol prefix and a
    /// fresh fixed-size nonce for each preparation attempt.
    #[test]
    fn readiness_handshakes_are_versioned_fixed_size_and_unique() {
        let first = readiness_handshake().unwrap();
        let second = readiness_handshake().unwrap();

        assert_eq!(first.len(), READINESS_HANDSHAKE_BYTES);
        assert!(first.starts_with(READINESS_HANDSHAKE_PREFIX));
        assert_ne!(first, second);
    }

    /// Verifies the standard OCI bundle applies every mandatory isolation
    /// layer.
    #[test]
    #[allow(clippy::too_many_lines)] // One assertion block reviews the complete standard OCI boundary.
    fn standard_bundle_has_mandatory_outer_isolation() {
        let fixture = Fixture::new();
        let created = fixture.create();
        assert_eq!(created.network_namespace().pid(), 4242);

        let paths = PodRuntimePaths::new(fixture.runtime.config().runtime_root(), &fixture.pod);
        let config = fixture.configuration();
        assert_eq!(config["linux"]["cgroupsPath"], "tascarrel/pod-1");
        let namespaces = config["linux"]["namespaces"]
            .as_array()
            .unwrap()
            .iter()
            .map(|namespace| namespace["type"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            namespaces,
            ["user", "mount", "pid", "ipc", "uts", "cgroup", "network"]
        );
        assert_eq!(config["process"]["noNewPrivileges"], true);
        assert_eq!(
            config["process"]["apparmorProfile"],
            STANDARD_APPARMOR_PROFILE
        );
        let blocked_syscalls = config["linux"]["seccomp"]["syscalls"][0]["names"]
            .as_array()
            .unwrap();
        for syscall in GLOBAL_BLOCKED_SYSCALLS
            .iter()
            .chain(CONTAINER_BLOCKED_SYSCALLS)
        {
            assert!(
                blocked_syscalls
                    .iter()
                    .any(|blocked| blocked.as_str() == Some(*syscall))
            );
        }
        let arguments = config["process"]["args"].as_array().unwrap();
        assert_eq!(arguments[1], "--ready-socket");
        assert_eq!(
            arguments[2],
            format!("{READINESS_SOCKET_DESTINATION}/{READINESS_SOCKET_FILE}")
        );
        assert_eq!(arguments[3], "--ready-handshake");
        assert_eq!(arguments[4], created.readiness_handshake());
        assert_eq!(
            created.readiness_handshake().len(),
            READINESS_HANDSHAKE_BYTES
        );
        assert!(
            created
                .readiness_handshake()
                .starts_with(READINESS_HANDSHAKE_PREFIX)
        );
        assert_eq!(arguments[5], "--init-directory");
        assert_eq!(arguments[6], "/run/tascarrel/hooks/init");
        assert!(!arguments.contains(&json!("--init-step")));

        let capabilities = config["process"]["capabilities"]["bounding"]
            .as_array()
            .unwrap()
            .iter()
            .map(|capability| capability.as_str().unwrap())
            .collect::<Vec<_>>();
        assert!(!capabilities.contains(&"CAP_SYS_ADMIN"));
        assert!(!capabilities.contains(&"CAP_NET_ADMIN"));
        for forbidden in FORBIDDEN_CAPABILITIES {
            assert!(!capabilities.contains(forbidden));
        }
        assert!(
            !config["linux"]["devices"]
                .as_array()
                .unwrap()
                .iter()
                .any(|device| device["path"] == "/dev/fuse")
        );
        for global_device in ["/dev/console", "/dev/ptmx"] {
            assert!(
                !config["linux"]["devices"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|device| device["path"] == global_device)
            );
        }
        assert_eq!(config["linux"]["resources"]["devices"][0]["allow"], false);
        assert_eq!(config["linux"]["resources"]["devices"][1]["allow"], true);
        assert_eq!(config["linux"]["resources"]["devices"][1]["type"], "a");
        assert_eq!(config["linux"]["resources"]["devices"][1]["access"], "rw");

        let mounts = config["mounts"].as_array().unwrap();
        assert!(
            !mounts
                .iter()
                .any(|mount| mount["destination"] == "/usr/local/bin/docker")
        );
        assert!(
            !mounts
                .iter()
                .any(|mount| mount["destination"] == "/usr/local/bin/nix")
        );
        let nix = mounts
            .iter()
            .find(|mount| mount["destination"] == "/nix/store")
            .unwrap();
        assert!(nix["options"].as_array().unwrap().contains(&json!("ro")));
        assert!(
            !mounts.iter().any(|mount| mount["destination"] == "/bin/sh"),
            "the runtime shell must not replace an image shell symlink target"
        );
        let resolv_conf = mounts
            .iter()
            .find(|mount| mount["destination"] == "/etc/resolv.conf")
            .unwrap();
        assert_eq!(
            fs::read(resolv_conf["source"].as_str().unwrap()).unwrap(),
            RESOLV_CONF
        );
        assert!(
            resolv_conf["options"]
                .as_array()
                .unwrap()
                .contains(&json!("ro"))
        );
        let hosts = mounts
            .iter()
            .find(|mount| mount["destination"] == "/etc/hosts")
            .unwrap();
        assert_eq!(fs::read(hosts["source"].as_str().unwrap()).unwrap(), HOSTS);
        assert!(hosts["options"].as_array().unwrap().contains(&json!("ro")));
        let cgroup = mounts
            .iter()
            .find(|mount| mount["destination"] == "/sys/fs/cgroup")
            .unwrap();
        assert!(cgroup["options"].as_array().unwrap().contains(&json!("ro")));
        assert!(!cgroup["options"].as_array().unwrap().contains(&json!("rw")));
        assert!(
            mounts
                .iter()
                .any(|mount| mount["destination"] == "/run" && mount["type"] == "tmpfs")
        );
        let temporary = mounts
            .iter()
            .find(|mount| mount["destination"] == "/tmp")
            .unwrap();
        assert_eq!(temporary["type"], "none");
        assert_eq!(
            temporary["source"],
            paths.temporary.to_string_lossy().as_ref()
        );
        for option in ["rbind", "rw", "rprivate", "nosuid", "nodev"] {
            assert!(
                temporary["options"]
                    .as_array()
                    .unwrap()
                    .contains(&json!(option))
            );
        }
        let host_dev = mounts
            .iter()
            .find(|mount| mount["destination"] == HOST_DEV_MOUNT)
            .unwrap();
        assert_eq!(host_dev["source"], POD_DEVICE_SOURCE_ROOT);
        assert!(
            host_dev["options"]
                .as_array()
                .unwrap()
                .contains(&json!("ro"))
        );
        assert!(
            !host_dev["options"]
                .as_array()
                .unwrap()
                .contains(&json!("nodev"))
        );
        let readiness = mounts
            .iter()
            .find(|mount| mount["destination"] == READINESS_SOCKET_DESTINATION)
            .unwrap();
        assert_eq!(
            readiness["source"],
            created.readiness_directory().to_str().unwrap()
        );
        let readiness_options = readiness["options"]
            .as_array()
            .unwrap()
            .iter()
            .map(|option| option.as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            readiness_options,
            ["bind", "ro", "rprivate", "nosuid", "nodev", "noexec"]
        );
        assert_eq!(
            fs::metadata(created.readiness_directory())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        let workspace = mounts
            .iter()
            .find(|mount| mount["destination"] == "/workspace")
            .unwrap();
        let docker = mounts
            .iter()
            .find(|mount| mount["destination"] == "/var/lib/docker")
            .unwrap();
        assert_ne!(workspace["source"], docker["source"]);
    }

    /// Verifies rootless-container facilities remain explicit and narrowly
    /// scoped.
    #[test]
    fn rootless_container_feature_is_explicit_and_narrow() {
        let fixture = Fixture::with_options(false, true);
        let image = ImageConfig::for_process(
            ["HOME=/home/develop"],
            ImageUser::new("develop", 1000, 1000, [999]).unwrap(),
            "/workspace",
        )
        .unwrap();
        fixture.create_with_config(&image);
        let config = fixture.configuration();
        assert_eq!(config["process"]["args"][5], "--rootless-uid");
        assert_eq!(config["process"]["args"][6], "1000");
        assert_eq!(config["process"]["args"][7], "--rootless-gid");
        assert_eq!(config["process"]["args"][8], "1000");
        for expected in ["/dev/fuse", "/dev/net/tun"] {
            assert!(
                config["linux"]["devices"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|device| device["path"] == expected),
                "missing {expected}"
            );
        }
        let cgroup = config["mounts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|mount| mount["destination"] == "/sys/fs/cgroup")
            .unwrap();
        assert!(cgroup["options"].as_array().unwrap().contains(&json!("rw")));
        assert_eq!(
            config["annotations"]["io.tascarrel.rootless-containers"],
            "true"
        );
        // Child mounts below procfs/sysfs make Linux reject a fresh procfs
        // mount from Podman's nested user namespace.
        assert!(
            config["linux"]["readonlyPaths"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert!(
            config["linux"]["maskedPaths"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            config["process"]["apparmorProfile"],
            CONTAINER_APPARMOR_PROFILE
        );
        let blocked_syscalls = config["linux"]["seccomp"]["syscalls"][0]["names"]
            .as_array()
            .unwrap();
        for syscall in GLOBAL_BLOCKED_SYSCALLS {
            assert!(
                blocked_syscalls
                    .iter()
                    .any(|blocked| blocked.as_str() == Some(*syscall))
            );
        }
        for syscall in CONTAINER_BLOCKED_SYSCALLS {
            assert!(
                !blocked_syscalls
                    .iter()
                    .any(|blocked| blocked.as_str() == Some(*syscall))
            );
        }
        for destination in ["/etc/subuid", "/etc/subgid"] {
            let subordinate_ids = config["mounts"]
                .as_array()
                .unwrap()
                .iter()
                .find(|mount| mount["destination"] == destination)
                .unwrap();
            assert_eq!(
                fs::read(subordinate_ids["source"].as_str().unwrap()).unwrap(),
                b"develop:65536:65536\n"
            );
            assert!(
                subordinate_ids["options"]
                    .as_array()
                    .unwrap()
                    .contains(&json!("ro"))
            );
        }
    }

    /// Verifies Podman implies rootless policy and injects its immutable
    /// runtime tools.
    #[test]
    fn podman_feature_implies_rootless_facilities_and_injects_runtime_tools() {
        let fixture = Fixture::with_policy(false, PodPolicy::default().with_podman(true));
        let image = ImageConfig::for_process(
            ["HOME=/home/develop"],
            ImageUser::new("develop", 1000, 1000, []).unwrap(),
            "/workspace",
        )
        .unwrap();
        fixture.create_with_config(&image);
        let config = fixture.configuration();
        let arguments = config["process"]["args"].as_array().unwrap();
        assert_eq!(arguments[5], "--rootless-uid");
        assert_eq!(arguments[6], "1000");
        let podman = config["mounts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|mount| mount["destination"] == PODMAN_PROGRAM_DESTINATION)
            .unwrap();
        assert!(
            podman["source"]
                .as_str()
                .unwrap()
                .ends_with("/opqr-podman/bin/podman")
        );
        assert!(
            podman["options"]
                .as_array()
                .unwrap()
                .iter()
                .any(|option| option == "ro")
        );
        for (destination, suffix) in [
            (NEWUIDMAP_PROGRAM_DESTINATION, "/stuv-shadow/bin/newuidmap"),
            (NEWGIDMAP_PROGRAM_DESTINATION, "/stuv-shadow/bin/newgidmap"),
        ] {
            let helper = config["mounts"]
                .as_array()
                .unwrap()
                .iter()
                .find(|mount| mount["destination"] == destination)
                .unwrap();
            assert!(helper["source"].as_str().unwrap().ends_with(suffix));
            assert!(
                helper["options"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|option| option == "ro")
            );
            assert!(
                helper["options"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|option| option == "nosuid")
            );
        }
        let containers_policy = config["mounts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|mount| mount["destination"] == CONTAINERS_POLICY_DESTINATION)
            .unwrap();
        assert_eq!(
            fs::read(containers_policy["source"].as_str().unwrap()).unwrap(),
            CONTAINERS_POLICY
        );
        for option in ["ro", "nosuid", "nodev"] {
            assert!(
                containers_policy["options"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|configured| configured == option)
            );
        }
        assert_eq!(
            config["annotations"]["io.tascarrel.rootless-containers"],
            "true"
        );
        assert_eq!(config["annotations"]["io.tascarrel.podman"], "true");
        assert_eq!(
            config["process"]["apparmorProfile"],
            CONTAINER_APPARMOR_PROFILE
        );
        assert!(
            !config["mounts"]
                .as_array()
                .unwrap()
                .iter()
                .any(|mount| mount["destination"] == "/usr/local/bin/docker")
        );
    }

    /// Verifies virtualization adds only the KVM device and keeps standard
    /// confinement.
    #[test]
    fn virtualization_exposes_the_kvm_device_only_when_enabled() {
        let fixture = Fixture::with_policy(false, PodPolicy::default().with_virtualization(true));
        fixture.create();
        let config = fixture.configuration();
        assert!(
            config["linux"]["devices"]
                .as_array()
                .unwrap()
                .iter()
                .any(|device| {
                    device["path"] == "/dev/kvm"
                        && device["major"] == KVM_DEVICE_MAJOR
                        && device["minor"] == KVM_DEVICE_MINOR
                })
        );
        assert!(
            config["linux"]["resources"]["devices"]
                .as_array()
                .unwrap()
                .iter()
                .any(|rule| {
                    rule["allow"] == true
                        && rule["type"] == "c"
                        && rule["major"] == KVM_DEVICE_MAJOR
                        && rule["minor"] == KVM_DEVICE_MINOR
                })
        );
        assert_eq!(config["annotations"]["io.tascarrel.virtualization"], "true");
        assert_eq!(
            config["process"]["apparmorProfile"],
            STANDARD_APPARMOR_PROFILE
        );
        let blocked_syscalls = config["linux"]["seccomp"]["syscalls"][0]["names"]
            .as_array()
            .unwrap();
        assert!(!blocked_syscalls.iter().any(|syscall| syscall == "ioctl"));
        assert!(!blocked_syscalls.iter().any(|syscall| syscall == "mmap"));

        let fixture = Fixture::with_policy(false, PodPolicy::default());
        fixture.create();
        let config = fixture.configuration();
        assert!(
            !config["linux"]["devices"]
                .as_array()
                .unwrap()
                .iter()
                .any(|device| device["path"] == "/dev/kvm")
        );
    }

    /// Verifies configured USB nodes use curated sources and a static
    /// allowlist.
    #[test]
    fn configured_usb_nodes_use_curated_sources_without_dynamic_mknod_access() {
        let node = PodDevice::from_source(
            "/dev/tascarrel/usb/board/tty",
            "/dev/ttyACM0",
            PodDeviceKind::Char,
            166,
            0,
        )
        .unwrap();
        let fixture = Fixture::with_devices(std::slice::from_ref(&node));
        let image = ImageConfig::for_process(
            ["HOME=/home/develop"],
            ImageUser::new("develop", 1000, 1000, []).unwrap(),
            "/workspace",
        )
        .unwrap();
        fixture.create_with_config(&image);
        let config = fixture.configuration();
        assert!(
            !config["linux"]["devices"]
                .as_array()
                .unwrap()
                .iter()
                .any(|device| device["path"] == node.path().to_string_lossy().as_ref()),
            "runc must not try to bind a stable alias that does not exist in the VM /dev"
        );
        let device_rules = config["linux"]["resources"]["devices"].as_array().unwrap();
        assert!(device_rules.iter().any(|rule| {
            rule["allow"] == true && rule["type"] == "a" && rule["access"] == "rw"
        }));
        assert!(!device_rules.iter().any(|rule| {
            rule["allow"] == true
                && rule["type"] == "c"
                && rule["major"] == 166
                && rule["minor"] == 0
                && rule["access"].as_str().unwrap().contains('m')
        }));
        let invocations = fixture.runner.invocations();
        let create = invocations
            .iter()
            .position(|invocation| {
                invocation.program == Path::new("/tools/runc")
                    && invocation
                        .arguments
                        .iter()
                        .any(|argument| argument == "create")
            })
            .expect("runc create was invoked");
        let link = invocations
            .iter()
            .position(|invocation| {
                invocation.program == Path::new("/tools/nsenter")
                    && invocation
                        .arguments
                        .iter()
                        .any(|argument| argument == "device-link")
            })
            .expect("pod creation links configured USB nodes after runc create");
        assert!(create < link);
        let link = &invocations[link];
        assert!(
            link.arguments
                .iter()
                .any(|argument| argument == "/dev/tascarrel/usb/board/tty")
        );
        assert!(
            link.arguments
                .iter()
                .any(|argument| argument == "/run/tascarrel/host-dev/ttyACM0")
        );
    }

    /// Verifies USB synchronization changes only the target pod's private
    /// devices.
    #[test]
    fn running_pod_usb_sync_updates_only_its_curated_private_dev_tree() {
        let fixture = Fixture::new();
        fixture.create();
        *fixture.runner.state.state.lock().unwrap() =
            Some(br#"{"ociVersion":"1.2.0","id":"pod-1","status":"running","pid":4242}"#.to_vec());
        let node = PodDevice::from_source(
            "/dev/tascarrel/usb/board/tty",
            "/dev/ttyACM0",
            PodDeviceKind::Char,
            166,
            0,
        )
        .unwrap();
        fixture
            .runtime
            .sync_devices(&fixture.pod, std::slice::from_ref(&node))
            .unwrap();
        let invocations = fixture.runner.invocations();
        assert!(!invocations.iter().any(|invocation| {
            invocation.program == Path::new("/tools/runc")
                && invocation
                    .arguments
                    .iter()
                    .any(|argument| argument == "update")
        }));
        let link = invocations
            .iter()
            .position(|invocation| {
                invocation.program == Path::new("/tools/nsenter")
                    && invocation
                        .arguments
                        .iter()
                        .any(|argument| argument == "device-link")
            })
            .unwrap();
        let arguments = &invocations[link].arguments;
        assert!(arguments.iter().any(|argument| argument == "--target"));
        assert!(arguments.iter().any(|argument| argument == "4242"));
        assert!(
            arguments
                .iter()
                .any(|argument| { argument.to_string_lossy().ends_with("/bundle/userns") })
        );
        assert!(arguments.iter().any(|argument| argument == "--root"));
        assert!(
            arguments
                .iter()
                .any(|argument| argument == "/dev/tascarrel/usb/board/tty")
        );
        assert!(
            arguments
                .iter()
                .any(|argument| argument == "/run/tascarrel/host-dev/ttyACM0")
        );
    }

    /// Verifies shares are idmapped and expand the image user's home directory.
    #[test]
    fn workspace_shares_are_idmapped_and_expand_the_image_users_home() {
        let ownership_probe = tempfile::tempdir().unwrap();
        let directory = OpenOptions::new()
            .read(true)
            .custom_flags(nix::libc::O_DIRECTORY)
            .open(ownership_probe.path())
            .unwrap();
        match nix::unistd::fchown(
            &directory,
            Some(nix::unistd::Uid::from_raw(1000)),
            Some(nix::unistd::Gid::from_raw(1000)),
        ) {
            Ok(()) => {}
            Err(nix::errno::Errno::EPERM | nix::errno::Errno::EINVAL) => return,
            Err(error) => panic!("probe image-user ownership failed unexpectedly: {error}"),
        }
        let fixture = Fixture::with_policy_and_shares(
            false,
            PodPolicy::default(),
            &[
                ("cargo-cache", "~/.cache/cargo"),
                ("build-cache", "/workspace/.cache/build"),
            ],
        );
        let image = ImageConfig::for_process(
            ["HOME=/home/develop"],
            ImageUser::new("develop", 1000, 1000, []).unwrap(),
            "/workspace",
        )
        .unwrap();
        fixture.create_with_config(&image);
        let home_parent = fs::metadata(fixture.mounts.root().join("home/develop/.cache")).unwrap();
        assert_eq!((home_parent.uid(), home_parent.gid()), (1000, 1000));
        let workspace_parent = fs::metadata(fixture.mounts.workspace().join(".cache")).unwrap();
        assert_eq!(
            (workspace_parent.uid(), workspace_parent.gid()),
            (1000, 1000)
        );
        let config = fixture.configuration();
        for (name, destination) in [
            ("cargo-cache", "/home/develop/.cache/cargo"),
            ("build-cache", "/workspace/.cache/build"),
        ] {
            let mount = config["mounts"]
                .as_array()
                .unwrap()
                .iter()
                .find(|mount| mount["destination"] == destination)
                .unwrap();
            assert!(
                mount["source"]
                    .as_str()
                    .unwrap()
                    .ends_with(&format!("share-{name}"))
            );
            assert_eq!(
                mount["options"],
                json!(["rbind", "rw", "rprivate", "nosuid", "nodev"])
            );
            assert!(fixture.runner.invocations().iter().any(|invocation| {
                invocation.arguments.iter().any(|argument| {
                    argument
                        .to_string_lossy()
                        .ends_with(&format!("shares/{name}"))
                }) && invocation
                    .arguments
                    .iter()
                    .any(|argument| argument == "--map-users")
            }));
        }
    }

    /// Verifies share destinations cannot overlap runtime-owned mounts.
    #[test]
    fn workspace_shares_reject_runtime_and_overlapping_destinations() {
        for shares in [
            vec![("runtime-cache", "/run/cache")],
            vec![("one", "~/.cache"), ("two", "~/.cache/nested")],
        ] {
            let fixture = Fixture::with_policy_and_shares(false, PodPolicy::default(), &shares);
            let image = ImageConfig::for_process(
                ["HOME=/home/develop"],
                ImageUser::new("develop", 1000, 1000, []).unwrap(),
                "/workspace",
            )
            .unwrap();
            let error = fixture
                .runtime
                .create_from_mounts_and_config(&fixture.pod, &fixture.mounts, &image)
                .unwrap_err();
            assert!(matches!(error, RuntimeError::InvalidConfig(_)));
            assert!(
                !fixture
                    .runtime
                    .config()
                    .runtime_root()
                    .join("pod-1")
                    .exists()
            );
        }
    }

    /// Verifies the agent harness destination remains outside runtime mounts.
    #[test]
    fn agent_harness_destination_does_not_overlap_runtime_mounts() {
        let share = PodShare::agent_harnesses("/nix/store/test-agent-harnesses").unwrap();
        assert_eq!(share.path, "/opt/tascarrel/harnesses");
        validate_share_destination(Path::new(&share.path), share.runtime_origin).unwrap();
    }

    /// Verifies chat attachments are exposed through a read-only idmapped
    /// mount.
    #[test]
    fn chat_attachments_use_a_read_only_idmapped_mount() {
        let share = PodShare::chat_attachments("/var/lib/tascarrel/chat/attachments").unwrap();
        assert_eq!(share.path, "/opt/tascarrel/chat/attachments");
        assert!(share.read_only);
        assert!(!share.runtime_origin);
        validate_share_destination(Path::new(&share.path), share.runtime_origin).unwrap();
    }

    /// Verifies the Code server runtime is mounted read-only.
    #[test]
    fn code_server_uses_a_read_only_runtime_mount() {
        let share = PodShare::code_server("/nix/store/test-code-server").unwrap();
        assert_eq!(share.path, "/opt/tascarrel/tools/code-server");
        assert!(share.read_only);
        assert!(share.runtime_origin);
        validate_share_destination(Path::new(&share.path), share.runtime_origin).unwrap();
    }

    /// Verifies agent context and skills are mounted read-only.
    #[test]
    fn workspace_agent_context_and_skills_use_read_only_runtime_mounts() {
        let agents = PodShare::workspace_agents("/var/lib/tascarrel/input/agents").unwrap();
        assert_eq!(agents.path, "/run/tascarrel/agents");
        assert!(agents.read_only);
        validate_share_destination(Path::new(&agents.path), agents.runtime_origin).unwrap();

        let skills =
            PodShare::workspace_agent_skills("/var/lib/tascarrel/input/agents/skills").unwrap();
        assert_eq!(skills.path, "~/.agents/skills");
        assert!(skills.read_only);
    }

    /// Verifies image environment values survive with runtime-owned overrides.
    #[test]
    fn image_environment_is_retained_with_runtime_owned_overrides() {
        let fixture = Fixture::new();
        let image = ImageConfig::for_process(
            [
                "PATH=/image/bin",
                "HOME=/home/develop",
                "USER=develop",
                "LOGNAME=develop",
                "IMAGE_DEFAULT=from-dockerfile",
                "DUP=first",
                "DUP=last",
                "IMAGE_RUNTIME_METADATA=image-value",
                "TASCARREL_POD_ID=image-value",
            ],
            ImageUser::new("develop", 1000, 1000, [999]).unwrap(),
            "/workspace",
        )
        .unwrap();
        fixture.create_with_config(&image);

        let configuration = fixture.configuration();
        assert_eq!(
            configuration["process"]["user"],
            json!({ "uid": 0, "gid": 0 })
        );
        let environment = configuration["process"]["env"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry.as_str().unwrap())
            .collect::<Vec<_>>();
        assert!(environment.contains(&"PATH=/image/bin"));
        assert!(environment.contains(&"IMAGE_DEFAULT=from-dockerfile"));
        assert!(environment.contains(&"DUP=last"));
        assert!(!environment.contains(&"DUP=first"));
        assert!(environment.contains(&"HOME=/root"));
        assert!(environment.contains(&"USER=root"));
        assert!(environment.contains(&"LOGNAME=root"));
        assert!(!environment.contains(&"HOME=/home/develop"));
        assert!(!environment.contains(&"USER=develop"));
        assert!(environment.contains(&"IMAGE_RUNTIME_METADATA=image-value"));
        assert!(environment.contains(&"TASCARREL_POD_ID=pod-1"));
        assert!(!environment.contains(&"TASCARREL_POD_ID=image-value"));
    }

    /// Verifies storage mappings and private runtime directory permissions.
    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "one assertion block reviews the complete namespace mount pipeline"
    )]
    fn storage_mounts_use_expected_mappings_and_runtime_state_is_private() {
        let fixture = Fixture::new();
        fixture.create();
        let config = fixture.configuration();
        assert!(config["linux"].get("uidMappings").is_none());
        assert!(config["linux"].get("gidMappings").is_none());
        let user_namespace = config["linux"]["namespaces"]
            .as_array()
            .unwrap()
            .iter()
            .find(|namespace| namespace["type"] == "user")
            .unwrap();
        assert!(
            user_namespace["path"]
                .as_str()
                .unwrap()
                .ends_with("/bundle/userns")
        );
        let mount_namespace = config["linux"]["namespaces"]
            .as_array()
            .unwrap()
            .iter()
            .find(|namespace| namespace["type"] == "mount")
            .unwrap();
        assert!(
            mount_namespace["path"]
                .as_str()
                .unwrap()
                .ends_with("/bundle/mountns")
        );
        let invocations = fixture.runner.invocations();
        let unshare = invocations
            .iter()
            .find(|invocation| invocation.program == Path::new("/tools/unshare"))
            .unwrap();
        assert!(
            unshare.arguments[0]
                .to_string_lossy()
                .ends_with("/bundle/userns")
        );
        assert!(
            unshare.arguments[0]
                .to_string_lossy()
                .starts_with("--user=")
        );
        assert!(
            unshare.arguments[1]
                .to_string_lossy()
                .ends_with("/bundle/mountns")
        );
        assert!(
            unshare.arguments[1]
                .to_string_lossy()
                .starts_with("--mount=")
        );
        assert_eq!(unshare.arguments[2], "--map-users=0:100000:131072");
        assert_eq!(unshare.arguments[3], "--map-groups=0:200000:131072");
        assert_eq!(&unshare.arguments[4..6], ["--propagation", "private"]);
        assert_eq!(unshare.arguments[6], "--");
        assert_eq!(unshare.arguments[7], TRUE_PROGRAM);
        assert_eq!(unshare.arguments.len(), 8);
        let mount_invocations = invocations
            .iter()
            .filter(|invocation| invocation.program == Path::new("/tools/nsenter"))
            .collect::<Vec<_>>();
        assert_eq!(mount_invocations.len(), 10);
        let expose = &mount_invocations[0].arguments;
        assert!(expose[0].to_string_lossy().ends_with("/bundle/mountns"));
        assert_eq!(
            &expose[1..6],
            ["--", "/tools/mount", "--no-canonicalize", "--bind", "--"]
        );
        assert!(expose[6].to_string_lossy().starts_with("/proc/1/root/"));
        assert!(expose[6].to_string_lossy().ends_with("/bundle/userns"));
        assert!(expose[7].to_string_lossy().ends_with("/bundle/userns"));
        assert_eq!(
            mount_invocations[1].arguments,
            [
                expose[0].clone(),
                OsString::from("--"),
                OsString::from("/tools/mount"),
                OsString::from("--make-private"),
                OsString::from("--"),
                expose[7].clone(),
            ]
        );
        for (pair, expected_source) in mount_invocations[2..].chunks_exact(2).zip([
            fixture.mounts.root(),
            fixture.mounts.workspace(),
            fixture.mounts.docker(),
            fixture.mounts.temporary(),
        ]) {
            let args = &pair[0].arguments;
            assert!(args[0].to_string_lossy().ends_with("/bundle/mountns"));
            assert_eq!(&args[1..5], ["--", "/tools/mount", "--bind", "--map-users"]);
            assert!(args[5].to_string_lossy().ends_with("/bundle/userns"));
            assert_eq!(args[6], "--");
            assert_eq!(args[7], expected_source.as_os_str());
            assert_eq!(
                pair[1].arguments,
                [
                    args[0].clone(),
                    OsString::from("--"),
                    OsString::from("/tools/mount"),
                    OsString::from("--make-private"),
                    OsString::from("--"),
                    args[8].clone(),
                ]
            );
        }
        let unshare_index = invocations
            .iter()
            .position(|invocation| invocation.program == Path::new("/tools/unshare"))
            .unwrap();
        let expose_index = invocations
            .iter()
            .position(|invocation| invocation == mount_invocations[0])
            .unwrap();
        let first_idmap_index = invocations
            .iter()
            .position(|invocation| invocation == mount_invocations[2])
            .unwrap();
        assert!(unshare_index < expose_index && expose_index < first_idmap_index);
        let paths = PodRuntimePaths::new(fixture.runtime.config().runtime_root(), &fixture.pod);
        let create = invocations
            .iter()
            .find(|invocation| {
                invocation.program == Path::new("/tools/runc")
                    && invocation
                        .arguments
                        .iter()
                        .any(|argument| argument == "create")
            })
            .unwrap();
        assert!(create.detached);
        assert_eq!(
            create.arguments,
            [
                OsString::from("--root"),
                fixture.runtime.config().runc_root().as_os_str().to_owned(),
                OsString::from("--log"),
                paths.runc_create_log.as_os_str().to_owned(),
                OsString::from("--log-format"),
                OsString::from("json"),
                OsString::from("create"),
                OsString::from("--bundle"),
                paths.bundle.as_os_str().to_owned(),
                fixture.pod.as_str().into(),
            ]
        );
        assert!(!paths.runc_create_log.exists());
        assert!(
            invocations
                .iter()
                .filter(|invocation| invocation.program == Path::new("/tools/runc"))
                .filter(|invocation| !invocation
                    .arguments
                    .iter()
                    .any(|argument| argument == "create"))
                .all(|invocation| !invocation.detached)
        );
        for (path, expected_mode) in [
            (fixture.runtime.config().runtime_root(), 0o711),
            (fixture.runtime.config().runc_root(), 0o700),
            (paths.pod.as_path(), 0o711),
            (paths.bundle.as_path(), 0o711),
            (paths.mounts.as_path(), 0o711),
            (paths.rootfs.as_path(), 0o700),
            (paths.workspace.as_path(), 0o700),
            (paths.docker.as_path(), 0o700),
            (paths.temporary.as_path(), 0o700),
            (paths.user_namespace.as_path(), 0o600),
            (paths.mount_namespace.as_path(), 0o600),
            (paths.resolv_conf.as_path(), 0o644),
            (paths.hosts.as_path(), 0o644),
            (paths.bundle.join(CONFIG_FILE).as_path(), 0o600),
        ] {
            let mode = fs::metadata(path).unwrap().permissions().mode() & 0o777;
            assert_eq!(
                mode,
                expected_mode,
                "unexpected mode for {}",
                path.display()
            );
        }
    }

    /// Verifies Docker's broad capabilities stay scoped by the outer user
    /// namespace.
    #[test]
    fn docker_capabilities_are_broad_only_inside_outer_user_namespace() {
        let fixture = Fixture::with_policy(false, PodPolicy::default().with_docker_daemon(true));
        fixture.create();
        let config = fixture.configuration();
        let capabilities = config["process"]["capabilities"]["bounding"]
            .as_array()
            .unwrap()
            .iter()
            .map(|capability| capability.as_str().unwrap())
            .collect::<Vec<_>>();
        for expected in [
            "CAP_SYS_ADMIN",
            "CAP_NET_ADMIN",
            "CAP_SYS_PTRACE",
            "CAP_MKNOD",
        ] {
            assert!(capabilities.contains(&expected));
        }
        for forbidden in FORBIDDEN_CAPABILITIES {
            assert!(!capabilities.contains(forbidden));
        }
        assert_eq!(config["process"]["noNewPrivileges"], false);
        assert_eq!(config["process"]["args"][5], "--nested-containers");
        let arguments = config["process"]["args"].as_array().unwrap();
        assert_eq!(arguments[6], "--start-docker");
        assert!(!arguments.contains(&json!("--init-step")));
        assert_eq!(
            config["process"]["apparmorProfile"],
            CONTAINER_APPARMOR_PROFILE
        );
        let blocked_syscalls = config["linux"]["seccomp"]["syscalls"][0]["names"]
            .as_array()
            .unwrap();
        for syscall in GLOBAL_BLOCKED_SYSCALLS {
            assert!(
                blocked_syscalls
                    .iter()
                    .any(|blocked| blocked.as_str() == Some(*syscall))
            );
        }
        for syscall in CONTAINER_BLOCKED_SYSCALLS {
            assert!(
                !blocked_syscalls
                    .iter()
                    .any(|blocked| blocked.as_str() == Some(*syscall))
            );
        }
        assert!(
            !config["linux"]["readonlyPaths"]
                .as_array()
                .unwrap()
                .iter()
                .any(|path| path == "/proc/sys")
        );
        assert!(
            config["linux"]["readonlyPaths"]
                .as_array()
                .unwrap()
                .iter()
                .any(|path| path == "/proc/sysrq-trigger")
        );
        assert!(
            config["linux"]["devices"]
                .as_array()
                .unwrap()
                .iter()
                .any(|device| device["path"] == "/dev/fuse")
        );
        let namespaces = config["linux"]["namespaces"].as_array().unwrap();
        assert!(
            namespaces
                .iter()
                .any(|namespace| namespace["type"] == "user")
        );
        assert!(
            namespaces
                .iter()
                .any(|namespace| namespace["type"] == "network")
        );
        assert!(
            config["mounts"]
                .as_array()
                .unwrap()
                .iter()
                .any(|mount| mount["destination"] == "/usr/local/bin/docker")
        );
        let cgroup = config["mounts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|mount| mount["destination"] == "/sys/fs/cgroup")
            .unwrap();
        assert!(cgroup["options"].as_array().unwrap().contains(&json!("rw")));
        assert!(!cgroup["options"].as_array().unwrap().contains(&json!("ro")));
        assert_eq!(
            config["annotations"]["io.tascarrel.nested-containers"],
            "true"
        );
        assert_eq!(config["annotations"]["io.tascarrel.docker-daemon"], "true");
    }

    /// Verifies Docker enables only its required nesting and resources.
    #[test]
    fn docker_feature_implies_nesting_and_injects_only_docker_resources() {
        let fixture = Fixture::with_policy(false, PodPolicy::default().with_docker_daemon(true));
        fixture.create();
        let config = fixture.configuration();
        let arguments = config["process"]["args"].as_array().unwrap();
        assert_eq!(arguments[5], "--nested-containers");
        assert_eq!(arguments[6], "--start-docker");
        assert_eq!(arguments[7], "--dockerd");
        assert!(
            arguments[8]
                .as_str()
                .unwrap()
                .ends_with("/ijkl-docker/bin/dockerd")
        );
        let docker = config["mounts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|mount| mount["destination"] == "/usr/local/bin/docker")
            .unwrap();
        assert!(
            docker["source"]
                .as_str()
                .unwrap()
                .ends_with("/mnop-docker-client/bin/docker")
        );
        assert!(docker["options"].as_array().unwrap().contains(&json!("ro")));
        assert_eq!(
            config["annotations"]["io.tascarrel.nested-containers"],
            "true"
        );
        assert_eq!(config["annotations"]["io.tascarrel.docker-daemon"], "true");
        assert!(
            !config["mounts"]
                .as_array()
                .unwrap()
                .iter()
                .any(|mount| mount["destination"] == "/nix/var/nix/daemon-socket")
        );
    }

    /// Verifies the Nix daemon exposes its store, socket, and private roots.
    #[test]
    fn nix_daemon_service_mounts_the_persistent_store_socket_and_private_roots() {
        let fixture = Fixture::with_policy(false, PodPolicy::default().with_nix_daemon(true));
        fixture.create();
        let config = fixture.configuration();
        let nix = config["mounts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|mount| mount["destination"] == "/usr/local/bin/nix")
            .unwrap();
        assert!(
            nix["source"]
                .as_str()
                .unwrap()
                .ends_with("/qrst-nix/bin/nix")
        );
        assert!(nix["options"].as_array().unwrap().contains(&json!("ro")));
        let socket = config["mounts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|mount| mount["destination"] == "/nix/var/nix/daemon-socket")
            .unwrap();
        assert!(
            socket["source"]
                .as_str()
                .unwrap()
                .ends_with("/run/pod-nix-daemon")
        );
        assert!(socket["options"].as_array().unwrap().contains(&json!("ro")));
        let gc_roots = config["mounts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|mount| {
                mount["destination"]
                    .as_str()
                    .is_some_and(|path| path.ends_with("/gcroots/tascarrel/pods/pod-1"))
            })
            .unwrap();
        assert!(
            gc_roots["source"]
                .as_str()
                .unwrap()
                .ends_with("/persistent-nix/nix/var/nix/gcroots/tascarrel/pods/pod-1")
        );
        assert_eq!(
            gc_roots["destination"],
            "/nix/var/nix/gcroots/tascarrel/pods/pod-1"
        );
        assert!(
            gc_roots["options"]
                .as_array()
                .unwrap()
                .contains(&json!("rw"))
        );
        assert!(
            !gc_roots["options"]
                .as_array()
                .unwrap()
                .contains(&json!("ro"))
        );
        assert_eq!(config["process"]["noNewPrivileges"], true);
        assert!(
            config["process"]["env"]
                .as_array()
                .unwrap()
                .contains(&json!(
                    "NIX_CONFIG=experimental-features = nix-command flakes"
                ))
        );
        assert_eq!(config["annotations"]["io.tascarrel.nix-daemon"], "true");
        let store = config["mounts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|mount| mount["destination"] == "/nix/store")
            .unwrap();
        assert!(
            store["source"]
                .as_str()
                .unwrap()
                .ends_with("/persistent-nix/nix/store")
        );
        assert!(store["options"].as_array().unwrap().contains(&json!("ro")));
        assert_eq!(
            config["annotations"]["io.tascarrel.nested-containers"],
            "false"
        );
    }

    /// Verifies the systemd cgroup manager is applied to every runc operation.
    #[test]
    fn systemd_cgroup_manager_is_used_consistently() {
        let fixture = Fixture::with_systemd_cgroup(true);
        fixture.create();
        assert_eq!(
            fixture.configuration()["linux"]["cgroupsPath"],
            "system.slice:tascarrel:pod-1"
        );
        let runc_invocations = fixture
            .runner
            .invocations()
            .into_iter()
            .filter(|invocation| invocation.program == Path::new("/tools/runc"))
            .collect::<Vec<_>>();
        assert!(!runc_invocations.is_empty());
        assert!(runc_invocations.iter().all(|invocation| {
            invocation
                .arguments
                .iter()
                .any(|argument| argument == "--systemd-cgroup")
        }));
    }

    /// Verifies failed creation releases completed mounts and bundle state.
    #[test]
    fn create_failure_unmounts_every_successful_mount_and_removes_bundle() {
        let fixture = Fixture::new();
        fixture.runner.fail_once("create --bundle");
        let error = fixture
            .runtime
            .create_from_mounts(&fixture.pod, &fixture.mounts)
            .unwrap_err();
        assert!(error.to_string().contains("create pod"));
        assert!(
            !fixture
                .runtime
                .config()
                .runtime_root()
                .join(fixture.pod.as_str())
                .exists()
        );
        let invocations = fixture.runner.invocations();
        let unmounts = invocations
            .iter()
            .filter(|invocation| invocation.program == Path::new("/tools/umount"))
            .collect::<Vec<_>>();
        assert_eq!(unmounts.len(), 2);
        assert!(
            unmounts[0].arguments[1]
                .to_string_lossy()
                .ends_with("/mountns")
        );
        assert!(
            unmounts[1].arguments[1]
                .to_string_lossy()
                .ends_with("/userns")
        );
        let rendered = invocations
            .iter()
            .map(|invocation| {
                invocation
                    .arguments
                    .iter()
                    .map(|argument| argument.to_string_lossy())
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .collect::<Vec<_>>();
        let create = rendered
            .iter()
            .position(|line| line.contains("create --bundle"))
            .unwrap();
        let delete = rendered
            .iter()
            .position(|line| line.ends_with("delete --force pod-1"))
            .unwrap();
        let list = rendered
            .iter()
            .position(|line| line.ends_with("list --format=json"))
            .unwrap();
        assert!(create < delete && delete < list);
    }

    /// Verifies live runc state prevents namespace cleanup after failed
    /// creation.
    #[test]
    fn failed_runc_create_retains_namespaces_when_force_delete_leaves_live_state() {
        let fixture = Fixture::new();
        fixture.runner.fail_once("create --bundle");
        fixture.runner.fail_once("delete --force pod-1");

        assert!(
            fixture
                .runtime
                .create_from_mounts(&fixture.pod, &fixture.mounts)
                .is_err()
        );
        assert!(
            fixture
                .runtime
                .config()
                .runtime_root()
                .join(fixture.pod.as_str())
                .is_dir()
        );
        assert!(
            fixture
                .runner
                .invocations()
                .iter()
                .all(|invocation| invocation.program != Path::new("/tools/umount"))
        );
    }

    /// Verifies an inconclusive absence check preserves pinned namespaces.
    #[test]
    fn failed_runc_create_retains_namespaces_when_absence_check_fails() {
        let fixture = Fixture::new();
        fixture.runner.fail_once("create --bundle");
        fixture.runner.fail_once("list --format=json");

        assert!(
            fixture
                .runtime
                .create_from_mounts(&fixture.pod, &fixture.mounts)
                .is_err()
        );
        assert!(
            fixture
                .runtime
                .config()
                .runtime_root()
                .join(fixture.pod.as_str())
                .is_dir()
        );
        assert!(
            fixture
                .runner
                .invocations()
                .iter()
                .all(|invocation| invocation.program != Path::new("/tools/umount"))
        );
    }

    /// Verifies partial mount failure releases only completed mounts.
    #[test]
    fn partial_mount_failure_only_unmounts_completed_mounts() {
        let fixture = Fixture::new();
        fixture.runner.fail_once("/storage/workspace");
        assert!(
            fixture
                .runtime
                .create_from_mounts(&fixture.pod, &fixture.mounts)
                .is_err()
        );
        let unmounts = fixture
            .runner
            .invocations()
            .into_iter()
            .filter(|invocation| invocation.program == Path::new("/tools/umount"))
            .collect::<Vec<_>>();
        assert_eq!(unmounts.len(), 2);
        assert!(
            unmounts[0].arguments[1]
                .to_string_lossy()
                .ends_with("/mountns")
        );
        assert!(
            unmounts[1].arguments[1]
                .to_string_lossy()
                .ends_with("/userns")
        );
    }

    /// Verifies propagation setup failure releases pinned namespaces.
    #[test]
    fn propagation_failure_releases_the_pinned_namespaces() {
        let fixture = Fixture::new();
        fixture.runner.fail_once(&format!(
            "--make-private -- {}",
            fixture
                .runtime
                .config()
                .runtime_root()
                .join(fixture.pod.as_str())
                .join(MOUNTS_DIRECTORY)
                .join(ROOTFS_MOUNT)
                .display()
        ));
        let error = fixture
            .runtime
            .create_from_mounts(&fixture.pod, &fixture.mounts)
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("make namespace-private idmapped bind mount private")
        );
        let invocations = fixture.runner.invocations();
        let unmounts = invocations
            .iter()
            .filter(|invocation| invocation.program == Path::new("/tools/umount"))
            .collect::<Vec<_>>();
        assert_eq!(unmounts.len(), 2);
        assert!(
            unmounts[0].arguments[1]
                .to_string_lossy()
                .ends_with("/mountns")
        );
        assert!(
            unmounts[1].arguments[1]
                .to_string_lossy()
                .ends_with("/userns")
        );
        assert!(!invocations.iter().any(|invocation| {
            invocation.program == Path::new("/tools/runc")
                && invocation
                    .arguments
                    .iter()
                    .any(|argument| argument == "create")
        }));
    }

    /// Verifies partial namespace creation attempts cleanup of both candidates.
    #[test]
    fn partial_namespace_creation_failure_attempts_both_candidate_unmounts() {
        let fixture = Fixture::new();
        fixture.runner.fail_once("--map-users=0:100000:131072");
        assert!(
            fixture
                .runtime
                .create_from_mounts(&fixture.pod, &fixture.mounts)
                .is_err()
        );
        assert!(
            !fixture
                .runtime
                .config()
                .runtime_root()
                .join(fixture.pod.as_str())
                .exists()
        );
        let unmounts = fixture
            .runner
            .invocations()
            .into_iter()
            .filter(|invocation| invocation.program == Path::new("/tools/umount"))
            .collect::<Vec<_>>();
        assert_eq!(unmounts.len(), 2);
        assert!(
            unmounts[0].arguments[1]
                .to_string_lossy()
                .ends_with("/mountns")
        );
        assert!(
            unmounts[1].arguments[1]
                .to_string_lossy()
                .ends_with("/userns")
        );
    }

    /// Verifies invalid created state is removed before local mounts.
    #[test]
    fn invalid_created_state_is_force_deleted_before_unmounting() {
        let fixture = Fixture::new();
        *fixture.runner.state.state.lock().unwrap() = Some(b"not-json".to_vec());
        assert!(
            fixture
                .runtime
                .create_from_mounts(&fixture.pod, &fixture.mounts)
                .is_err()
        );
        let invocations = fixture.runner.invocations();
        let delete = invocations
            .iter()
            .position(|invocation| {
                invocation.program == Path::new("/tools/runc")
                    && invocation
                        .arguments
                        .iter()
                        .any(|argument| argument == "delete")
            })
            .unwrap();
        let first_unmount = invocations
            .iter()
            .position(|invocation| invocation.program == Path::new("/tools/umount"))
            .unwrap();
        assert!(delete < first_unmount);
    }

    /// Verifies lifecycle operations pass exact arguments without a shell.
    #[test]
    fn lifecycle_uses_exact_non_shell_argument_vectors() {
        let fixture = Fixture::new();
        fixture.create();
        fixture.runtime.start(&fixture.pod).unwrap();
        fixture.runtime.destroy(&fixture.pod).unwrap();
        let rendered = fixture
            .runner
            .invocations()
            .iter()
            .map(|invocation| {
                invocation
                    .arguments
                    .iter()
                    .map(|value| value.to_string_lossy())
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .collect::<Vec<_>>();
        assert!(rendered.iter().any(|line| line.ends_with("start pod-1")));
        assert!(
            rendered
                .iter()
                .any(|line| line.ends_with("delete --force pod-1"))
        );
    }

    /// Verifies null runc inventory confirms a retried deletion is complete.
    #[test]
    fn delete_retry_treats_a_null_runc_list_as_confirmed_empty() {
        let fixture = Fixture::new();
        fixture.create();
        *fixture.runner.state.list.lock().unwrap() = b"null".to_vec();
        fixture.runner.fail_once("delete --force pod-1");

        fixture.runtime.destroy(&fixture.pod).unwrap();

        assert!(
            !fixture
                .runtime
                .config()
                .runtime_root()
                .join(fixture.pod.as_str())
                .exists()
        );
        let rendered = fixture
            .runner
            .invocations()
            .iter()
            .map(|invocation| {
                invocation
                    .arguments
                    .iter()
                    .map(|value| value.to_string_lossy())
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .collect::<Vec<_>>();
        assert!(
            rendered
                .iter()
                .any(|line| line.ends_with("list --format=json"))
        );
    }

    /// Verifies failed deletion preserves mounts while runc retains the pod.
    #[test]
    fn failed_delete_retains_mounts_while_runc_still_lists_the_pod() {
        let fixture = Fixture::new();
        fixture.create();
        fixture.runner.fail_once("delete --force pod-1");

        assert!(fixture.runtime.destroy(&fixture.pod).is_err());

        assert!(
            fixture
                .runtime
                .config()
                .runtime_root()
                .join(fixture.pod.as_str())
                .is_dir()
        );
        assert!(
            fixture
                .runner
                .invocations()
                .iter()
                .all(|invocation| invocation.program != Path::new("/tools/umount"))
        );
    }

    /// Verifies cleanup tolerates namespace pins released by an earlier
    /// attempt.
    #[test]
    fn cleanup_accepts_a_namespace_pin_already_released_by_an_interrupted_attempt() {
        let fixture = Fixture::new();
        fixture.create();
        let mount_namespace = fixture
            .runtime
            .config()
            .runtime_root()
            .join(fixture.pod.as_str())
            .join(BUNDLE_DIRECTORY)
            .join(MOUNT_NAMESPACE_FILE);
        fixture
            .runner
            .fail_once(&format!("umount -- {}", mount_namespace.display()));

        fixture.runtime.destroy(&fixture.pod).unwrap();
        assert!(
            !fixture
                .runtime
                .config()
                .runtime_root()
                .join(fixture.pod.as_str())
                .exists()
        );
        let unmounts = fixture
            .runner
            .invocations()
            .into_iter()
            .filter(|invocation| invocation.program == Path::new("/tools/umount"))
            .collect::<Vec<_>>();
        assert_eq!(unmounts.len(), 2);
        assert_eq!(unmounts[0].arguments[1], mount_namespace.as_os_str());
        assert!(
            unmounts[1].arguments[1]
                .to_string_lossy()
                .ends_with("/userns")
        );
    }

    /// Verifies reverse unmounting stops after a namespace failure.
    #[test]
    fn reverse_unmount_stops_before_user_namespace_after_mount_namespace_failure() {
        let user_namespace = Path::new("/runtime/userns");
        let mount_namespace = Path::new("/runtime/mountns");
        let mut attempted = Vec::new();

        let result = unmount_in_reverse(
            &[user_namespace, mount_namespace],
            |mountpoint| -> Result<(), &'static str> {
                attempted.push(mountpoint.to_path_buf());
                if mountpoint == mount_namespace {
                    Err("still mounted")
                } else {
                    Ok(())
                }
            },
        );

        assert_eq!(result, Err("still mounted"));
        assert_eq!(attempted, [mount_namespace]);
    }

    /// Verifies kernel mountinfo escape sequences decode safely.
    #[test]
    fn mountinfo_path_decoder_handles_kernel_escaping() {
        assert_eq!(
            decode_mountinfo_path(br"/run/tascarrel/a\040b\011c\134d").unwrap(),
            b"/run/tascarrel/a b\tc\\d"
        );
        assert!(decode_mountinfo_path(br"/truncated\04").is_err());
        assert!(decode_mountinfo_path(br"/invalid\999").is_err());
    }

    /// Verifies source symlinks cannot escape validated pod storage.
    #[test]
    fn source_symlinks_fail_closed() {
        let fixture = Fixture::new();
        let link = fixture.temporary.path().join("root-link");
        symlink(fixture.mounts.root(), &link).unwrap();
        let mounts = PodMounts::new(
            &link,
            fixture.mounts.workspace(),
            fixture.mounts.docker(),
            fixture.mounts.temporary(),
        )
        .unwrap();
        assert!(
            fixture
                .runtime
                .create_from_mounts(&fixture.pod, &mounts)
                .is_err()
        );
    }
}
