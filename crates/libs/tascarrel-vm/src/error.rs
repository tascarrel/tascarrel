//! Public configuration and VM lifecycle error contracts.
//!
//! [`VmError`] is the primary runtime error contract; [`ConfigError`] describes
//! invalid [`crate::VmConfig`] construction.

use std::io;
use std::path::PathBuf;
use std::process::ExitStatus;
use std::time::Duration;

use thiserror::Error;

/// An error while starting, controlling, or stopping a QEMU VM.
#[derive(Debug, Error)]
pub enum VmError {
    /// The managed QEMU invocation could not be encoded.
    #[error("failed to encode the managed QEMU invocation")]
    Invocation,
    /// The private QEMU machine-protocol channel failed.
    #[error("QEMU machine-protocol operation failed")]
    Qmp,
    /// A required host executable failed discovery or version probing.
    #[error("required {name} executable {program} is unavailable: {reason}")]
    RequiredExecutableUnavailable {
        /// Human-readable executable role.
        name: &'static str,
        /// Configured executable path or program name.
        program: PathBuf,
        /// Discovery or version-probe failure.
        reason: String,
    },
    /// The immutable system image does not exist or is not a regular file.
    #[error("system disk image is not a regular file: {0}")]
    InvalidSystemDiskImage(PathBuf),
    /// Sparse raw data-disk preparation failed.
    #[error("failed to prepare persistent data disk {path}")]
    PrepareDataDisk {
        /// The managed sparse raw image path.
        path: PathBuf,
    },
    /// The blocking data-disk preparation task failed.
    #[error("persistent data-disk preparation task failed for {path}: {source}")]
    PrepareDataDiskTask {
        /// The managed sparse raw image path.
        path: PathBuf,
        /// The blocking task failure.
        #[source]
        source: tokio::task::JoinError,
    },
    /// A configured shared host path does not exist or is not a directory.
    #[error("shared host path for mount tag {mount_tag:?} is not a directory: {path}")]
    InvalidSharedDirectory {
        /// Guest-visible shared-directory mount tag.
        mount_tag: String,
        /// Configured host path.
        path: PathBuf,
    },
    /// Virtiofsd could not be spawned for a configured directory.
    #[error("failed to spawn virtiofsd executable {program} for mount tag {mount_tag:?}: {source}")]
    SpawnVirtiofsd {
        /// Guest-visible virtiofs mount tag.
        mount_tag: String,
        /// Executable that was invoked.
        program: PathBuf,
        /// Underlying operating-system error.
        #[source]
        source: io::Error,
    },
    /// Virtiofsd could not be inspected while waiting for readiness.
    #[error("failed to inspect virtiofsd for mount tag {mount_tag:?}: {source}")]
    InspectVirtiofsd {
        /// Guest-visible virtiofs mount tag.
        mount_tag: String,
        /// Underlying operating-system error.
        #[source]
        source: io::Error,
    },
    /// Virtiofsd exited before QEMU could use its vhost-user socket.
    #[error("virtiofsd for mount tag {mount_tag:?} exited before becoming ready: {status}")]
    VirtiofsdExitedBeforeReady {
        /// Guest-visible virtiofs mount tag.
        mount_tag: String,
        /// Virtiofsd exit status.
        status: ExitStatus,
    },
    /// Virtiofsd did not create its vhost-user socket before the deadline.
    #[error(
        "virtiofsd for mount tag {mount_tag:?} did not become ready within {timeout:?}: {socket}"
    )]
    VirtiofsdReadinessTimeout {
        /// Guest-visible virtiofs mount tag.
        mount_tag: String,
        /// Readiness deadline.
        timeout: Duration,
        /// Expected vhost-user socket path.
        socket: PathBuf,
    },
    /// A virtiofsd socket could not be inspected for a non-transient reason.
    #[error("failed to inspect virtiofsd socket {path}: {source}")]
    InspectVirtiofsdSocket {
        /// Expected vhost-user socket path.
        path: PathBuf,
        /// Underlying operating-system error.
        #[source]
        source: io::Error,
    },
    /// The configured direct-boot kernel does not exist or is not regular.
    #[error("kernel image is not a regular file: {0}")]
    InvalidKernel(PathBuf),
    /// The configured direct-boot initrd does not exist or is not regular.
    #[error("initrd image is not a regular file: {0}")]
    InvalidInitrd(PathBuf),
    /// A pre-existing managed path was not a Unix socket, so it was preserved.
    #[error("refusing to replace non-socket runtime path: {0}")]
    UnsafeRuntimeSocketPath(PathBuf),
    /// Another process has a live listener at a managed runtime socket path.
    #[error("runtime socket is already in use: {0}")]
    RuntimeSocketInUse(PathBuf),
    /// A pre-existing socket could not be safely classified as live or stale.
    #[error("failed to probe existing runtime socket {path}: {source}")]
    ProbeRuntimeSocket {
        /// Managed runtime path.
        path: PathBuf,
        /// Underlying operating-system error.
        #[source]
        source: io::Error,
    },
    /// The socket node was replaced while its liveness was being checked.
    #[error("runtime socket changed while being probed; refusing to unlink it: {0}")]
    RuntimeSocketChanged(PathBuf),
    /// Filesystem preparation for a managed runtime path failed.
    #[error("failed to prepare runtime path {path}: {source}")]
    PrepareRuntimePath {
        /// Managed runtime path.
        path: PathBuf,
        /// Underlying operating-system error.
        #[source]
        source: io::Error,
    },
    /// The runtime directory was not an owner-only real directory.
    #[error("VM runtime directory must be an owner-only real directory: {0}")]
    UnsafeRuntimeDirectory(PathBuf),
    /// QEMU could not be spawned.
    #[error("failed to spawn QEMU executable {program}: {source}")]
    Spawn {
        /// Executable that was invoked.
        program: PathBuf,
        /// Underlying operating-system error.
        #[source]
        source: io::Error,
    },
    /// VM lifecycle management requires an active Tokio runtime.
    #[error("QEMU lifecycle requires an active Tokio runtime")]
    MissingRuntime,
    /// QEMU exited before its control channel became ready.
    #[error("QEMU exited before its control channel became ready: {status}")]
    ExitedBeforeReady {
        /// QEMU exit status.
        status: ExitStatus,
    },
    /// The startup deadline elapsed before the control channel was usable.
    #[error("QEMU control channel did not become ready within {timeout:?}: {socket}")]
    ReadinessTimeout {
        /// Readiness deadline.
        timeout: Duration,
        /// Expected socket path.
        socket: PathBuf,
    },
    /// The control socket could not be connected for a non-transient reason.
    #[error("failed to connect to QEMU control socket {path}: {source}")]
    ConnectControl {
        /// Socket path.
        path: PathBuf,
        /// Underlying operating-system error.
        #[source]
        source: io::Error,
    },
    /// Process inspection or waiting failed.
    #[error("failed to inspect or wait for QEMU: {0}")]
    Wait(#[source] io::Error),
    /// A termination signal could not be delivered.
    #[error("failed to signal QEMU process {pid}: {source}")]
    Signal {
        /// QEMU process identifier.
        pid: u32,
        /// Underlying operating-system error.
        #[source]
        source: io::Error,
    },
    /// The VM had already exited when an operation required a running process.
    #[error("QEMU is no longer running")]
    NotRunning,
}

/// A validation error produced while constructing a [`crate::VmConfig`].
#[derive(Debug, Error)]
pub enum ConfigError {
    /// No immutable system disk image was supplied.
    #[error("a VM system disk image is required")]
    MissingSystemDiskImage,
    /// No managed persistent data disk was supplied.
    #[error("a persistent VM data disk and minimum size are required")]
    MissingDataDisk,
    /// A zero-size persistent data disk cannot hold guest state.
    #[error("persistent VM data disk size must be greater than zero")]
    ZeroDataDiskSize,
    /// The persistent data disk and system image paths are identical.
    #[error("the persistent data disk must differ from the system disk image: {0}")]
    DataDiskIsSystemImage(PathBuf),
    /// No transient VM runtime directory was supplied.
    #[error("a VM runtime directory is required")]
    MissingRuntimeDirectory,
    /// The runtime directory path was not absolute.
    #[error("VM runtime directory path must be absolute: {0}")]
    RelativeRuntimeDirectory(PathBuf),
    /// The runtime directory cannot be represented in QEMU options.
    #[error("VM runtime directory path is not valid UTF-8: {0:?}")]
    NonUtf8RuntimeDirectory(PathBuf),
    /// Host architecture detection failed.
    #[error("host architecture is unsupported")]
    UnsupportedHostArchitecture,
    /// Memory was configured as zero MiB.
    #[error("VM memory must be greater than zero MiB")]
    ZeroMemory,
    /// The virtual CPU count was zero.
    #[error("VM virtual CPU count must be greater than zero")]
    ZeroVcpus,
    /// A timeout was zero and would therefore never permit the operation.
    #[error("{name} timeout must be greater than zero")]
    ZeroTimeout {
        /// Human-readable timeout name.
        name: &'static str,
    },
    /// The guest-visible virtio-serial port name is invalid.
    #[error("invalid virtio-serial port name `{0}`; use ASCII letters, digits, '.', '_' or '-'")]
    InvalidControlPortName(String),
    /// A Unix socket path is longer than the portable `sockaddr_un` limit.
    #[error("Unix socket path is too long ({length} bytes; maximum {maximum}): {path}")]
    SocketPathTooLong {
        /// Supplied socket path.
        path: PathBuf,
        /// Encoded path length.
        length: usize,
        /// Maximum accepted encoded path length.
        maximum: usize,
    },
    /// The persistent disk path cannot be represented in QEMU's JSON syntax.
    #[error("persistent disk image path is not valid UTF-8: {0:?}")]
    NonUtf8DataDiskPath(PathBuf),
    /// The immutable system disk path cannot be represented in QEMU's JSON
    /// syntax.
    #[error("system disk image path is not valid UTF-8: {0:?}")]
    NonUtf8SystemDiskPath(PathBuf),
    /// A shared host directory path was not absolute.
    #[error("shared host directory path must be absolute: {0}")]
    RelativeSharedDirectoryPath(PathBuf),
    /// A shared host directory path could not be represented in QEMU options.
    #[error("shared host directory path is not valid UTF-8: {0:?}")]
    NonUtf8SharedDirectoryPath(PathBuf),
    /// A shared-directory mount tag was empty or contained a NUL byte.
    #[error("shared directory mount tag must be non-empty and contain no NUL bytes: {0:?}")]
    InvalidSharedDirectoryMountTag(String),
    /// A shared-directory mount tag cannot fit in every supported transport.
    #[error(
        "shared directory mount tag is too long ({length} bytes; maximum {maximum}): {mount_tag:?}"
    )]
    SharedDirectoryMountTagTooLong {
        /// Supplied guest-visible tag.
        mount_tag: String,
        /// Encoded tag length.
        length: usize,
        /// Maximum supported encoded length.
        maximum: usize,
    },
    /// More than one shared directory used the same guest-visible mount tag.
    #[error("shared directory mount tag is configured more than once: {0:?}")]
    DuplicateSharedDirectoryMountTag(String),
    /// No direct Linux kernel boot configuration was supplied.
    #[error("direct kernel boot configuration is required")]
    MissingDirectKernelBoot,
    /// A direct Linux boot needs a non-empty kernel command line.
    #[error("direct kernel boot requires a non-empty kernel command line")]
    EmptyKernelCommandLine,
}
