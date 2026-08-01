//! Concrete runc integration shared by pod lifecycle and process execution.

use std::collections::BTreeMap;
use std::fs;
use std::fs::OpenOptions;
use std::io;
use std::io::Read;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::fs::chown;
use std::os::unix::net::UnixListener;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;
use std::sync::RwLock;

use nix::sys::stat::Mode;
use nix::sys::stat::fchmod;
use nix::unistd::Gid;
use nix::unistd::Uid;
use reportify::ErrorExt as _;
use reportify::Report;
use reportify::ResultExt as _;
use tascarrel_sharefs::DirectoryEntry;

use super::share_overlays::PreparedShareOverlay;
use super::share_overlays::ShareOverlayRuntime;
use super::share_overlays::ShareOverlayRuntimeConfig;
use crate::runtime::pod::ContainerStatus;
use crate::runtime::pod::CreatedPod;
use crate::runtime::pod::ImageConfig;
use crate::runtime::pod::POD_ID_MAP_SIZE;
use crate::runtime::pod::PodDevice;
use crate::runtime::pod::PodId as RuntimePodId;
use crate::runtime::pod::PodPolicy;
use crate::runtime::pod::PodPrograms;
use crate::runtime::pod::PodRuntime;
use crate::runtime::pod::PodShare;
use crate::runtime::pod::PodStorage;
use crate::runtime::pod::RuntimeConfig;
use crate::runtime::pod::RuntimeError;

const FIRST_IDENTITY: u32 = 1_000_000;
/// Highest slot whose two ID ranges fit below the kernel's maximum ID.
pub(crate) const MAX_IDENTITY_SLOT: u32 =
    (u32::MAX - FIRST_IDENTITY - (POD_ID_MAP_SIZE - 1)) / POD_ID_MAP_SIZE;

/// Immutable paths and policy used for every runc container.
#[derive(Clone, Debug)]
pub struct RuncConfig {
    /// Ephemeral OCI bundle directory.
    pub runtime_root: PathBuf,
    /// Ephemeral runc state directory.
    pub runc_root: PathBuf,
    /// Immutable programs mounted into each container.
    pub programs: PodPrograms,
    /// Absolute runc executable path.
    pub runc: PathBuf,
    /// Absolute mount executable path.
    pub mount: PathBuf,
    /// Absolute unmount executable path.
    pub umount: PathBuf,
    /// Absolute unshare executable path.
    pub unshare: PathBuf,
    /// Absolute nsenter executable path.
    pub nsenter: PathBuf,
    /// Absolute IP executable path.
    pub ip: PathBuf,
    /// Parent cgroup used for pod scopes.
    pub cgroup_parent: String,
    /// Whether runc uses the systemd cgroup driver.
    pub systemd_cgroup: bool,
    /// Workspace features applied to pod processes.
    pub policy: PodPolicy,
    /// Environment values added to the image environment.
    pub environment: BTreeMap<String, String>,
    /// Nix store exposed inside Nix-enabled pods.
    pub pod_nix_store: PathBuf,
    /// Host directory containing the Nix daemon socket.
    pub nix_daemon_socket_dir: PathBuf,
    /// Guest root containing Nix GC roots.
    pub nix_gc_root_dir: PathBuf,
    /// Same-filesystem staging directory for withdrawn Nix GC roots.
    pub nix_gc_root_trash_dir: PathBuf,
    /// Container path containing pod-specific Nix GC roots.
    pub pod_nix_gc_root_dir: PathBuf,
    /// Workspace CA trust configuration.
    pub workspace_ca: Option<WorkspaceCaConfig>,
    /// Additional immutable workspace shares.
    pub shares: Vec<PodShare>,
    /// Per-pod copy-on-write host-share runtime.
    pub share_overlays: ShareOverlayRuntimeConfig,
}

impl RuncConfig {
    /// Creates a runc configuration with stable guest-system tool paths.
    #[must_use]
    pub fn new(
        runtime_root: impl Into<PathBuf>,
        runc_root: impl Into<PathBuf>,
        programs: PodPrograms,
    ) -> Self {
        Self {
            runtime_root: runtime_root.into(),
            runc_root: runc_root.into(),
            programs,
            runc: "/run/current-system/sw/bin/runc".into(),
            mount: "/run/current-system/sw/bin/mount".into(),
            umount: "/run/current-system/sw/bin/umount".into(),
            unshare: "/run/current-system/sw/bin/unshare".into(),
            nsenter: "/run/current-system/sw/bin/nsenter".into(),
            ip: "/run/current-system/sw/bin/ip".into(),
            cgroup_parent: "tascarrel".to_owned(),
            systemd_cgroup: true,
            policy: PodPolicy::default(),
            environment: BTreeMap::new(),
            pod_nix_store: "/nix/store".into(),
            nix_daemon_socket_dir: "/nix/var/nix/daemon-socket".into(),
            nix_gc_root_dir: "/nix/var/nix/gcroots/tascarrel/pods".into(),
            nix_gc_root_trash_dir: "/nix/var/nix/tascarrel-gc-root-trash".into(),
            pod_nix_gc_root_dir: "/nix/var/nix/gcroots/tascarrel/pods".into(),
            workspace_ca: None,
            shares: Vec::new(),
            share_overlays: ShareOverlayRuntimeConfig::default(),
        }
    }
}

/// Workspace CA source, trust bundle paths, and filesystem bounds.
#[derive(Clone, Debug)]
pub struct WorkspaceCaConfig {
    /// Public workspace CA certificate on the guest filesystem.
    pub certificate: PathBuf,
    /// Relative paths to trust bundles within each pod root.
    pub bundle_paths: Vec<PathBuf>,
    /// Maximum accepted workspace CA size.
    pub max_certificate_bytes: u64,
    /// Maximum accepted size of an existing trust bundle.
    pub max_bundle_bytes: u64,
}

impl WorkspaceCaConfig {
    /// Creates a workspace CA configuration with common Linux trust bundles.
    #[must_use]
    pub fn new(certificate: impl Into<PathBuf>) -> Self {
        Self {
            certificate: certificate.into(),
            bundle_paths: [
                "etc/ssl/certs/ca-certificates.crt",
                "etc/ssl/cert.pem",
                "etc/pki/tls/certs/ca-bundle.crt",
                "etc/pki/ca-trust/extracted/pem/tls-ca-bundle.pem",
            ]
            .into_iter()
            .map(PathBuf::from)
            .collect(),
            max_certificate_bytes: 64 * 1024,
            max_bundle_bytes: 16 * 1024 * 1024,
        }
    }
}

/// Process execution coordinates derived from durable pod storage.
#[derive(Clone, Debug)]
pub(crate) struct PodExecution {
    /// Image user name.
    pub(crate) user: String,
    /// Host-mapped user identifier.
    pub(crate) uid: u32,
    /// Host-mapped group identifier.
    pub(crate) gid: u32,
    /// Image home directory.
    pub(crate) home: PathBuf,
    /// Image login shell.
    pub(crate) shell: PathBuf,
    /// Container execution coordinates.
    pub(crate) container: Option<ContainerExecution>,
}

/// Stable runc and OCI defaults used to execute inside one pod.
#[derive(Clone, Debug)]
pub(crate) struct ContainerExecution {
    /// Absolute runc executable used for process execution.
    pub(crate) runc: PathBuf,
    /// Runc state root containing the container.
    pub(crate) root: PathBuf,
    /// Whether runc uses its systemd cgroup driver.
    pub(crate) systemd_cgroup: bool,
    /// Runtime identifier of the container.
    pub(crate) id: String,
    /// Container-local user identifier.
    pub(crate) uid: u32,
    /// Container-local primary group identifier.
    pub(crate) gid: u32,
    /// Container-local supplementary group identifiers.
    pub(crate) additional_gids: Vec<u32>,
    /// Default working directory inside the container.
    pub(crate) working_directory: PathBuf,
    /// Effective process environment inherited from the image and workspace.
    pub(crate) environment: BTreeMap<String, String>,
    /// Workspace process restrictions applied by the executor.
    pub(crate) policy: PodPolicy,
    /// Pod-specific Nix direct-root path, when Nix is enabled.
    pub(crate) nix_gc_root: Option<PathBuf>,
}

/// Result of preparing a pod's runc container before it is started.
pub(crate) struct PreparedPod {
    /// Host process identifier of the prepared container init process.
    pub(crate) pid: u32,
    /// Authenticated per-attempt readiness listener and handshake.
    pub(crate) readiness: PreparedReadiness,
}

/// One per-pod readiness endpoint, removed when the attempt finishes.
pub(crate) struct PreparedReadiness {
    listener: Option<UnixListener>,
    socket: PathBuf,
    pub(crate) handshake: Vec<u8>,
    pub(crate) pid: u32,
    pub(crate) uid: u32,
    pub(crate) gid: u32,
}

impl PreparedReadiness {
    /// Takes the nonblocking listener while retaining socket cleanup ownership.
    pub(crate) fn take_listener(&mut self) -> Result<UnixListener, Report<RuntimeError>> {
        self.listener.take().ok_or_else(|| {
            RuntimeError::InvalidConfig("pod readiness listener was already consumed".to_owned())
                .report()
        })
    }
}

impl Drop for PreparedReadiness {
    fn drop(&mut self) {
        self.listener.take();
        match fs::remove_file(&self.socket) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => tracing::warn!(
                path = %self.socket.display(),
                %error,
                "could not remove pod readiness socket"
            ),
        }
    }
}

/// The one concrete pod runtime used by guestd.
pub(crate) struct Runc {
    config: RuncConfig,
    devices: RwLock<Vec<PodDevice>>,
    share_overlays: ShareOverlayRuntime,
}

impl Runc {
    /// Creates the concrete runc integration.
    pub(crate) fn new(config: RuncConfig) -> Result<Self, Report<RuntimeError>> {
        Self::runtime_from(&config, 0, &[])?;
        let share_overlays = ShareOverlayRuntime::open(config.share_overlays.clone())?;
        Ok(Self {
            config,
            devices: RwLock::new(Vec::new()),
            share_overlays,
        })
    }

    /// Returns the immutable Nix-store Bash injected into every pod.
    pub(crate) fn setup_shell(&self) -> &std::path::Path {
        self.config.programs.shell()
    }

    /// Prepares a stopped pod and returns its transient runtime coordinates.
    pub(crate) fn prepare(
        &self,
        storage: &PodStorage,
        slot: u32,
    ) -> Result<PreparedPod, Report<RuntimeError>> {
        let devices = device_snapshot(&self.devices)?;
        let identity = identity_for_slot(slot)?;
        let image = effective_image_config(storage.image_config(), &self.config.environment)?;
        let (uid, gid) = mapped_pod_identity(identity, image.user().uid(), image.user().gid())?;
        let overlay_shares = self.share_overlays.mount(storage.id(), uid, gid)?;
        let prepared = (|| {
            let runtime =
                Self::runtime_from_with_shares(&self.config, slot, &devices, &overlay_shares)?;
            if let Some(workspace_ca) = &self.config.workspace_ca {
                install_workspace_ca(storage.root(), workspace_ca)?;
            }
            let created = runtime
                .create_from_mounts_and_config(
                    storage.id(),
                    &crate::runtime::pod::PodMounts::try_from(storage).report()?,
                    &image,
                )
                .report()?;
            let finalized = prepared_readiness(&created, identity).map(|readiness| PreparedPod {
                pid: created.network_namespace().pid(),
                readiness,
            });
            match finalized {
                Ok(prepared) => Ok(prepared),
                Err(cause) => match runtime.destroy(storage.id()) {
                    Ok(()) => Err(cause),
                    Err(rollback) => Err(RuntimeError::RollbackFailed {
                        operation: "finalize prepared pod",
                        cause: cause.to_string(),
                        rollback: rollback.to_string(),
                    }
                    .report()),
                },
            }
        })();
        if let Err(cause) = &prepared
            && let Err(rollback) = self.share_overlays.unmount(storage.id())
        {
            return Err(RuntimeError::RollbackFailed {
                operation: "prepare pod ShareFS overlays",
                cause: cause.to_string(),
                rollback: rollback.to_string(),
            }
            .report());
        }
        prepared
    }

    /// Starts a prepared pod.
    pub(crate) fn start(&self, pod: &RuntimePodId, slot: u32) -> Result<(), Report<RuntimeError>> {
        Self::runtime_from(&self.config, slot, &[])?
            .start(pod)
            .report()
    }

    /// Confirms runc still reports the authenticated readiness peer as this
    /// pod's running init process.
    pub(crate) fn confirm_ready(
        &self,
        pod: &RuntimePodId,
        slot: u32,
        expected_pid: u32,
    ) -> Result<(), Report<RuntimeError>> {
        let runtime = Self::runtime_from(&self.config, slot, &[])?;
        let actual_pid = running_pid(&runtime, pod, "after readiness handshake").report()?;
        if actual_pid != expected_pid {
            return Err(RuntimeError::InvalidState {
                pod: pod.clone(),
                reason: format!(
                    "readiness peer PID {expected_pid} does not match current init PID {actual_pid}"
                ),
            }
            .report());
        }
        Ok(())
    }

    /// Removes a pod's transient runc state and mounts.
    pub(crate) fn stop(&self, pod: &RuntimePodId, slot: u32) -> Result<(), Report<RuntimeError>> {
        let runtime = Self::runtime_from(&self.config, slot, &[])?;
        if self.config.runtime_root.join(pod.as_str()).exists() {
            runtime.destroy(pod).report()?;
        } else {
            delete_runc_state(&self.config, pod)?;
        }
        self.share_overlays.unmount(pod)
    }

    /// Removes durable `ShareFS` upper state after pod teardown.
    pub(crate) fn destroy_share_overlays(
        &self,
        pod: &RuntimePodId,
    ) -> Result<(), Report<RuntimeError>> {
        self.share_overlays.destroy(pod)
    }

    /// Freezes and snapshots one pod's overlay share for host approval.
    pub(crate) fn prepare_share_overlay(
        &self,
        pod: &RuntimePodId,
        share: &str,
    ) -> Result<PreparedShareOverlay, Report<RuntimeError>> {
        self.share_overlays.prepare_approval(pod, share)
    }

    /// Reads one directory from a pod's overlay host-share view.
    pub(crate) fn read_share_overlay_directory(
        &self,
        pod: &RuntimePodId,
        share: &str,
        path: &Path,
    ) -> Result<Vec<DirectoryEntry>, Report<RuntimeError>> {
        self.share_overlays.read_directory(pod, share, path)
    }

    /// Opens one regular file from a pod's overlay host-share view.
    pub(crate) fn open_share_overlay_file(
        &self,
        pod: &RuntimePodId,
        share: &str,
        path: &Path,
    ) -> Result<fs::File, Report<RuntimeError>> {
        self.share_overlays.open_file(pod, share, path)
    }

    /// Returns process execution coordinates without checking container state.
    pub(crate) fn execution(
        &self,
        storage: &PodStorage,
        slot: u32,
    ) -> Result<PodExecution, Report<RuntimeError>> {
        let image = effective_image_config(storage.image_config(), &self.config.environment)?;
        let mut environment = image_environment(&image);
        environment.insert(
            "TASCARREL_POD_ID".to_owned(),
            storage.id().as_str().to_owned(),
        );
        let home = environment.get("HOME").map_or_else(
            || {
                if image.user().uid() == 0 {
                    PathBuf::from("/root")
                } else {
                    PathBuf::from("/workspace")
                }
            },
            PathBuf::from,
        );
        let shell = environment.get("SHELL").map_or_else(
            || self.config.programs.terminal_shell().to_path_buf(),
            PathBuf::from,
        );
        let identity = identity_for_slot(slot)?;
        let (uid, gid) = mapped_pod_identity(identity, image.user().uid(), image.user().gid())?;
        Ok(PodExecution {
            user: image.user().name().to_owned(),
            uid,
            gid,
            home,
            shell,
            container: Some(ContainerExecution {
                runc: self.config.runc.clone(),
                root: self.config.runc_root.clone(),
                systemd_cgroup: self.config.systemd_cgroup,
                id: storage.id().as_str().to_owned(),
                uid: image.user().uid(),
                gid: image.user().gid(),
                additional_gids: image.user().additional_gids().to_vec(),
                working_directory: image.working_directory().into(),
                environment,
                policy: self.config.policy,
                nix_gc_root: self
                    .config
                    .policy
                    .nix_daemon()
                    .then(|| self.config.pod_nix_gc_root_dir.join(storage.id().as_str())),
            }),
        })
    }

    /// Replaces the device set captured by future pod preparations.
    pub(crate) fn store_devices(
        &self,
        devices: Vec<PodDevice>,
    ) -> Result<(), Report<RuntimeError>> {
        replace_device_snapshot(&self.devices, devices)
    }

    /// Applies the latest stored device set to one newly running pod.
    pub(crate) fn sync_current_devices(
        &self,
        pod: &RuntimePodId,
        slot: u32,
    ) -> Result<(), Report<RuntimeError>> {
        let devices = device_snapshot(&self.devices)?;
        if devices.is_empty() {
            return Ok(());
        }
        Self::runtime_from(&self.config, slot, &devices)?
            .sync_devices(pod, &devices)
            .report()
    }

    /// Returns the bounded runc startup log, or an empty log before the pod is
    /// prepared and after its transient runtime has been removed.
    pub(crate) fn startup_log(
        &self,
        pod: &RuntimePodId,
        slot: u32,
    ) -> Result<Vec<u8>, Report<RuntimeError>> {
        let runtime = Self::runtime_from(&self.config, slot, &[])?;
        match runtime.startup_log(pod) {
            Ok(log) => Ok(log),
            Err(RuntimeError::NotPrepared(_)) => Ok(Vec::new()),
            Err(error) => Err(error.report()),
        }
    }

    /// Returns whether guestd has local runtime state for one pod.
    pub(crate) fn has_local_state(&self, pod: &RuntimePodId) -> bool {
        self.config.runtime_root.join(pod.as_str()).exists()
    }

    /// Creates the low-level runtime for one durable identity slot.
    fn runtime_from(
        config: &RuncConfig,
        slot: u32,
        devices: &[PodDevice],
    ) -> Result<PodRuntime, Report<RuntimeError>> {
        Self::runtime_from_with_shares(config, slot, devices, &[])
    }

    fn runtime_from_with_shares(
        config: &RuncConfig,
        slot: u32,
        devices: &[PodDevice],
        extra_shares: &[PodShare],
    ) -> Result<PodRuntime, Report<RuntimeError>> {
        let identity = identity_for_slot(slot)?;
        let runtime = RuntimeConfig::new(
            &config.runtime_root,
            &config.runc_root,
            config.programs.clone(),
            identity,
            identity,
        )
        .report()?;
        let runtime = runtime
            .with_programs(
                &config.runc,
                &config.mount,
                &config.umount,
                &config.unshare,
                &config.nsenter,
                &config.ip,
            )
            .report()?;
        let runtime = runtime
            .with_cgroup_parent(config.cgroup_parent.clone())
            .report()?;
        let runtime = runtime
            .with_nix_service(
                config.pod_nix_store.clone(),
                config.nix_daemon_socket_dir.clone(),
                config.nix_gc_root_dir.clone(),
                config.pod_nix_gc_root_dir.clone(),
            )
            .report()?;
        let runtime = runtime
            .with_shares(
                config
                    .shares
                    .iter()
                    .chain(extra_shares)
                    .cloned()
                    .collect::<Vec<_>>(),
            )
            .report()?;
        let runtime = runtime
            .with_devices(devices.iter().cloned())
            .report()?
            .with_systemd_cgroup(config.systemd_cgroup)
            .with_policy(config.policy);
        PodRuntime::open(runtime).report()
    }
}

/// Replaces the latest device set shared by reconciliation and pod starts.
fn replace_device_snapshot(
    devices: &RwLock<Vec<PodDevice>>,
    replacement: Vec<PodDevice>,
) -> Result<(), Report<RuntimeError>> {
    *devices
        .write()
        .map_err(|_| RuntimeError::LockPoisoned)
        .report()? = replacement;
    Ok(())
}

/// Clones the latest device set after any lifecycle serialization delay.
fn device_snapshot(
    devices: &RwLock<Vec<PodDevice>>,
) -> Result<Vec<PodDevice>, Report<RuntimeError>> {
    Ok(devices
        .read()
        .map_err(|_| RuntimeError::LockPoisoned)
        .report()?
        .clone())
}

/// Returns the outer identity range base for one durable slot.
fn identity_for_slot(slot: u32) -> Result<u32, Report<RuntimeError>> {
    slot.checked_mul(POD_ID_MAP_SIZE)
        .and_then(|offset| FIRST_IDENTITY.checked_add(offset))
        .filter(|base| base.checked_add(POD_ID_MAP_SIZE - 1).is_some())
        .ok_or_else(|| RuntimeError::InvalidConfig("pod identity slots are exhausted".to_owned()))
        .report()
}

/// Maps one image user into its pod's outer identity range.
fn mapped_pod_identity(
    identity: u32,
    uid: u32,
    gid: u32,
) -> Result<(u32, u32), Report<RuntimeError>> {
    let uid = identity
        .checked_add(uid)
        .ok_or_else(|| RuntimeError::InvalidConfig("mapped pod UID overflowed".to_owned()))
        .report()?;
    let gid = identity
        .checked_add(gid)
        .ok_or_else(|| RuntimeError::InvalidConfig("mapped pod GID overflowed".to_owned()))
        .report()?;
    Ok((uid, gid))
}

/// Merges workspace environment values into immutable image defaults.
fn effective_image_config(
    image: &ImageConfig,
    environment: &BTreeMap<String, String>,
) -> Result<ImageConfig, Report<RuntimeError>> {
    let mut merged = image_environment(image);
    merged.extend(environment.clone());
    ImageConfig::for_process(
        merged
            .into_iter()
            .map(|(name, value)| format!("{name}={value}")),
        image.user().clone(),
        image.working_directory(),
    )
    .map_err(|error| RuntimeError::InvalidConfig(error.to_string()))
    .report()
}

/// Converts validated OCI environment entries to a map.
fn image_environment(config: &ImageConfig) -> BTreeMap<String, String> {
    config
        .environment()
        .iter()
        .map(|entry| {
            let (name, value) = entry
                .split_once('=')
                .expect("ImageConfig validates environment entries");
            (name.to_owned(), value.to_owned())
        })
        .collect()
}

/// Deletes stale runc state when the local bundle was already lost.
fn delete_runc_state(config: &RuncConfig, pod: &RuntimePodId) -> Result<(), Report<RuntimeError>> {
    let mut command = Command::new(&config.runc);
    command.arg("--root").arg(&config.runc_root);
    if config.systemd_cgroup {
        command.arg("--systemd-cgroup");
    }
    let output = command
        .args(["delete", "--force", pod.as_str()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|source| RuntimeError::CommandStart {
            operation: "delete stale pod",
            program: config.runc.clone(),
            source,
        })
        .report()?;
    if output.status.success() {
        Ok(())
    } else {
        Err(RuntimeError::CommandFailed {
            operation: "delete stale pod",
            detail: String::from_utf8_lossy(&output.stderr)
                .trim()
                .chars()
                .take(4096)
                .collect(),
        }
        .report())
    }
}

/// Secures and binds the one listener visible only to this pod's mapped root.
fn prepared_readiness(
    created: &CreatedPod,
    identity: u32,
) -> Result<PreparedReadiness, Report<RuntimeError>> {
    let directory_path = created.readiness_directory();
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_CLOEXEC | nix::libc::O_DIRECTORY | nix::libc::O_NOFOLLOW)
        .open(directory_path)
        .map_err(|source| readiness_io("open pod readiness directory", directory_path, source))
        .report()?;
    nix::unistd::fchown(
        &directory,
        Some(Uid::from_raw(identity)),
        Some(Gid::from_raw(identity)),
    )
    .map_err(|source| {
        readiness_io(
            "set pod readiness directory ownership",
            directory_path,
            io::Error::from_raw_os_error(source as i32),
        )
    })
    .report()?;
    fchmod(&directory, Mode::from_bits_truncate(0o700))
        .map_err(|source| {
            readiness_io(
                "secure pod readiness directory",
                directory_path,
                io::Error::from_raw_os_error(source as i32),
            )
        })
        .report()?;

    let socket = created.readiness_socket();
    let listener = UnixListener::bind(&socket)
        .map_err(|source| readiness_io("bind pod readiness socket", &socket, source))
        .report()?;
    // A UnixStream FD refers to the socket object rather than its pathname
    // inode. The guestd-owned 0700 bundle ancestry cannot be swapped before
    // runc start, so secure the actual socket directory entry by path.
    chown(&socket, Some(identity), Some(identity))
        .map_err(|source| readiness_io("set pod readiness socket ownership", &socket, source))
        .report()?;
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))
        .map_err(|source| readiness_io("secure pod readiness socket", &socket, source))
        .report()?;
    listener
        .set_nonblocking(true)
        .map_err(|source| readiness_io("make pod readiness socket nonblocking", &socket, source))
        .report()?;

    Ok(PreparedReadiness {
        listener: Some(listener),
        socket,
        handshake: created.readiness_handshake().as_bytes().to_vec(),
        pid: created.network_namespace().pid(),
        uid: identity,
        gid: identity,
    })
}

fn readiness_io(
    operation: &'static str,
    path: &std::path::Path,
    source: io::Error,
) -> RuntimeError {
    RuntimeError::Io {
        operation,
        path: path.to_owned(),
        source,
    }
}

/// Adds the public workspace CA to common in-image trust bundles.
fn install_workspace_ca(
    root: &Path,
    config: &WorkspaceCaConfig,
) -> Result<(), Report<RuntimeError>> {
    if config.bundle_paths.is_empty()
        || config.max_certificate_bytes == 0
        || config.max_bundle_bytes == 0
    {
        return Err(RuntimeError::InvalidConfig(
            "workspace CA configuration requires a bundle path and non-zero filesystem bounds"
                .to_owned(),
        )
        .report());
    }
    let certificate = read_workspace_ca(root, &config.certificate, config.max_certificate_bytes)?;
    let bundle_paths = workspace_ca_bundle_paths(root, &config.bundle_paths)?;
    let public_roots =
        first_public_root_bundle(&bundle_paths, &certificate, config.max_bundle_bytes)?;
    for path in bundle_paths {
        extend_workspace_ca_bundle(
            &path,
            public_roots.as_deref(),
            &certificate,
            config.max_bundle_bytes,
        )?;
    }
    Ok(())
}

/// Reads a bounded regular CA after confirming the pod root is still safe.
fn read_workspace_ca(
    root: &Path,
    ca: &Path,
    max_certificate_bytes: u64,
) -> Result<Vec<u8>, Report<RuntimeError>> {
    let root_metadata = fs::symlink_metadata(root)
        .map_err(|source| workspace_ca_io("inspect pod root", root, source))
        .report()?;
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        return Err(RuntimeError::UnsafePath(root.to_owned()).report());
    }
    let mut ca_file = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_CLOEXEC | nix::libc::O_NOFOLLOW)
        .open(ca)
        .map_err(|source| workspace_ca_io("open workspace CA", ca, source))
        .report()?;
    let metadata = ca_file
        .metadata()
        .map_err(|source| workspace_ca_io("inspect workspace CA", ca, source))
        .report()?;
    if !metadata.is_file() || metadata.len() > max_certificate_bytes {
        return Err(RuntimeError::UnsafePath(ca.to_owned()).report());
    }
    let mut certificate = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or(0));
    Read::by_ref(&mut ca_file)
        .take(max_certificate_bytes.saturating_add(1))
        .read_to_end(&mut certificate)
        .map_err(|source| workspace_ca_io("read workspace CA", ca, source))
        .report()?;
    if certificate.is_empty()
        || u64::try_from(certificate.len()).map_or(true, |length| length > max_certificate_bytes)
    {
        return Err(RuntimeError::InvalidConfig(
            "workspace CA must contain bounded certificate data".to_owned(),
        )
        .report());
    }
    Ok(certificate)
}

/// Resolves common bundle paths whose image-provided parents are safe.
fn workspace_ca_bundle_paths(
    root: &Path,
    configured_paths: &[PathBuf],
) -> Result<Vec<PathBuf>, Report<RuntimeError>> {
    let mut bundle_paths = Vec::with_capacity(configured_paths.len());
    for relative in configured_paths {
        if relative.as_os_str().is_empty()
            || relative.is_absolute()
            || relative
                .components()
                .any(|component| !matches!(component, Component::Normal(_)))
        {
            return Err(RuntimeError::InvalidConfig(format!(
                "workspace CA bundle path must be relative and normalized: {}",
                relative.display()
            ))
            .report());
        }
        let path = root.join(relative);
        let parent = path
            .parent()
            .expect("pod root joined with a normalized relative path has a parent");
        if ensure_real_directories(root, parent)? {
            bundle_paths.push(path);
        }
    }
    Ok(bundle_paths)
}

/// Returns the first existing bundle that contains more than the workspace CA.
fn first_public_root_bundle(
    bundle_paths: &[PathBuf],
    certificate: &[u8],
    max_bundle_bytes: u64,
) -> Result<Option<Vec<u8>>, Report<RuntimeError>> {
    for path in bundle_paths {
        let mut options = OpenOptions::new();
        options
            .read(true)
            .custom_flags(nix::libc::O_CLOEXEC | nix::libc::O_NOFOLLOW);
        let mut file = match options.open(path) {
            Ok(file) => file,
            Err(error)
                if error.kind() == io::ErrorKind::NotFound
                    || error.raw_os_error() == Some(nix::libc::ELOOP) =>
            {
                continue;
            }
            Err(source) => {
                return Err(workspace_ca_io("open existing pod CA bundle", path, source).report());
            }
        };
        let metadata = file
            .metadata()
            .map_err(|source| workspace_ca_io("inspect existing pod CA bundle", path, source))
            .report()?;
        if !metadata.is_file() || metadata.len() > max_bundle_bytes {
            return Err(RuntimeError::UnsafePath(path.clone()).report());
        }
        let mut existing = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or(0));
        file.read_to_end(&mut existing)
            .map_err(|source| workspace_ca_io("read existing pod CA bundle", path, source))
            .report()?;
        if !existing.is_empty() && existing != certificate {
            return Ok(Some(existing));
        }
    }
    Ok(None)
}

/// Extends one regular bundle while retaining a fallback set of public roots.
fn extend_workspace_ca_bundle(
    path: &Path,
    public_roots: Option<&[u8]>,
    certificate: &[u8],
    max_bundle_bytes: u64,
) -> Result<(), Report<RuntimeError>> {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .append(true)
        .create(true)
        .mode(0o644)
        .custom_flags(nix::libc::O_CLOEXEC | nix::libc::O_NOFOLLOW);
    let mut file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.raw_os_error() == Some(nix::libc::ELOOP) => return Ok(()),
        Err(source) => return Err(workspace_ca_io("open pod CA bundle", path, source).report()),
    };
    let file_metadata = file
        .metadata()
        .map_err(|source| workspace_ca_io("inspect pod CA bundle", path, source))
        .report()?;
    if !file_metadata.is_file() || file_metadata.len() > max_bundle_bytes {
        return Err(RuntimeError::UnsafePath(path.to_owned()).report());
    }
    let mut existing = Vec::with_capacity(usize::try_from(file_metadata.len()).unwrap_or(0));
    file.read_to_end(&mut existing)
        .map_err(|source| workspace_ca_io("read pod CA bundle", path, source))
        .report()?;
    let mut has_content = !existing.is_empty();
    let mut has_certificate = existing
        .windows(certificate.len())
        .any(|window| window == certificate);
    if (existing.is_empty() || existing == certificate)
        && let Some(public_roots) = public_roots
    {
        append_bundle_data(&mut file, path, &mut has_content, public_roots)?;
        has_certificate |= public_roots
            .windows(certificate.len())
            .any(|window| window == certificate);
    }
    if !has_certificate {
        append_bundle_data(&mut file, path, &mut has_content, certificate)?;
    }
    file.sync_all()
        .map_err(|source| workspace_ca_io("synchronize pod CA bundle", path, source))
        .report()?;
    Ok(())
}

/// Creates missing bundle parents without following paths supplied by an image.
fn ensure_real_directories(root: &Path, parent: &Path) -> Result<bool, Report<RuntimeError>> {
    let relative = parent
        .strip_prefix(root)
        .map_err(|_| RuntimeError::UnsafePath(parent.to_owned()))
        .report()?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        if !matches!(component, Component::Normal(_)) {
            return Err(RuntimeError::UnsafePath(parent.to_owned()).report());
        }
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Ok(_) => return Ok(false),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                fs::create_dir(&current)
                    .map_err(|source| {
                        workspace_ca_io("create pod CA bundle directory", &current, source)
                    })
                    .report()?;
            }
            Err(source) => {
                return Err(
                    workspace_ca_io("inspect pod CA bundle directory", &current, source).report(),
                );
            }
        }
    }
    Ok(true)
}

/// Appends one PEM sequence with a line boundary from existing content.
fn append_bundle_data(
    file: &mut fs::File,
    path: &Path,
    has_content: &mut bool,
    data: &[u8],
) -> Result<(), Report<RuntimeError>> {
    if *has_content {
        file.write_all(b"\n")
            .map_err(|source| workspace_ca_io("separate pod CA certificates", path, source))
            .report()?;
    }
    file.write_all(data)
        .map_err(|source| workspace_ca_io("append pod CA bundle data", path, source))
        .report()?;
    *has_content = true;
    Ok(())
}

/// Builds a path-scoped runtime error for workspace CA filesystem operations.
fn workspace_ca_io(operation: &'static str, path: &Path, source: io::Error) -> RuntimeError {
    RuntimeError::Io {
        operation,
        path: path.to_owned(),
        source,
    }
}

/// Returns the live init PID at one readiness checkpoint.
fn running_pid(
    runtime: &PodRuntime,
    pod: &RuntimePodId,
    checkpoint: &str,
) -> Result<u32, RuntimeError> {
    let state = runtime.state(pod)?;
    if state.status() == &ContainerStatus::Running {
        state.pid().ok_or_else(|| RuntimeError::InvalidState {
            pod: pod.clone(),
            reason: format!("running pod has no init PID {checkpoint}"),
        })
    } else {
        Err(RuntimeError::InvalidState {
            pod: pod.clone(),
            reason: format!(
                "pod exited {checkpoint}: runc reported {:?}",
                state.status()
            ),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::Barrier;
    use std::thread;

    use super::*;
    use crate::runtime::pod::PodDeviceKind;

    /// Verifies every readiness guard unlinks its one-shot socket even when
    /// startup exits through an error path.
    #[test]
    fn readiness_guard_unlinks_its_socket() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("ready.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let readiness = PreparedReadiness {
            listener: Some(listener),
            socket: socket.clone(),
            handshake: Vec::new(),
            pid: 1,
            uid: 0,
            gid: 0,
        };

        drop(readiness);

        assert!(matches!(
            fs::symlink_metadata(socket),
            Err(error) if error.kind() == io::ErrorKind::NotFound
        ));
    }

    /// Verifies a delayed reconciler snapshots the newest device update rather
    /// than the stale set supplied when that reconciler began.
    #[test]
    fn delayed_device_sync_observes_latest_snapshot() {
        let devices = Arc::new(RwLock::new(Vec::new()));
        let first_stored = Arc::new(Barrier::new(2));
        let second_stored = Arc::new(Barrier::new(2));
        let first = PodDevice::new("/dev/first", PodDeviceKind::Char, 1, 3).unwrap();
        let second = PodDevice::new("/dev/second", PodDeviceKind::Char, 1, 5).unwrap();

        let delayed_devices = Arc::clone(&devices);
        let delayed_first_stored = Arc::clone(&first_stored);
        let delayed_second_stored = Arc::clone(&second_stored);
        let delayed = thread::spawn(move || {
            replace_device_snapshot(&delayed_devices, vec![first]).unwrap();
            delayed_first_stored.wait();
            delayed_second_stored.wait();
            device_snapshot(&delayed_devices).unwrap()
        });

        first_stored.wait();
        replace_device_snapshot(&devices, vec![second.clone()]).unwrap();
        second_stored.wait();

        assert_eq!(delayed.join().unwrap(), [second]);
        assert_eq!(
            device_snapshot(&devices).unwrap()[0].path(),
            Path::new("/dev/second")
        );
    }

    /// Verifies the workspace CA is added once without following image
    /// symlinks or replacing existing public roots.
    #[test]
    fn workspace_ca_extends_common_bundles_without_following_symlinks() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("root");
        fs::create_dir_all(root.join("etc/ssl/certs")).unwrap();
        fs::write(
            root.join("etc/ssl/certs/ca-certificates.crt"),
            b"PUBLIC ROOT\n",
        )
        .unwrap();
        fs::create_dir_all(root.join("etc/ssl")).unwrap();
        let outside = directory.path().join("outside");
        fs::write(&outside, b"DO NOT MODIFY\n").unwrap();
        symlink(&outside, root.join("etc/ssl/cert.pem")).unwrap();
        let ca = directory.path().join("workspace-ca.pem");
        let certificate = b"-----BEGIN CERTIFICATE-----\nTASCARREL\n-----END CERTIFICATE-----\n";
        fs::write(&ca, certificate).unwrap();

        let workspace_ca = WorkspaceCaConfig::new(&ca);
        install_workspace_ca(&root, &workspace_ca).unwrap();
        install_workspace_ca(&root, &workspace_ca).unwrap();

        let debian = fs::read(root.join("etc/ssl/certs/ca-certificates.crt")).unwrap();
        assert!(debian.starts_with(b"PUBLIC ROOT\n"));
        assert_eq!(
            debian
                .windows(certificate.len())
                .filter(|window| *window == certificate)
                .count(),
            1
        );
        assert_eq!(fs::read(&outside).unwrap(), b"DO NOT MODIFY\n");
        assert!(root.join("etc/ssl/cert.pem").is_symlink());
        let rhel = fs::read(root.join("etc/pki/tls/certs/ca-bundle.crt")).unwrap();
        assert!(rhel.starts_with(b"PUBLIC ROOT\n"));
        assert!(rhel.ends_with(certificate));
        let extracted =
            fs::read(root.join("etc/pki/ca-trust/extracted/pem/tls-ca-bundle.pem")).unwrap();
        assert!(extracted.starts_with(b"PUBLIC ROOT\n"));
        assert!(extracted.ends_with(certificate));

        let rhel_root = directory.path().join("rhel-root");
        let rhel_bundle = rhel_root.join("etc/pki/tls/certs/ca-bundle.crt");
        fs::create_dir_all(rhel_bundle.parent().unwrap()).unwrap();
        fs::write(&rhel_bundle, b"RHEL PUBLIC ROOT\n").unwrap();
        install_workspace_ca(&rhel_root, &workspace_ca).unwrap();
        let canonical = fs::read(rhel_root.join("etc/ssl/certs/ca-certificates.crt")).unwrap();
        assert!(canonical.starts_with(b"RHEL PUBLIC ROOT\n"));
        assert!(canonical.ends_with(certificate));
    }
}
