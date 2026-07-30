//! Validated configuration for managed Tascarrel virtual machines.
//!
//! [`VmConfig`] is the primary interface and is constructed through
//! [`VmConfigBuilder`]. Every valid configuration boots a host kernel and
//! initrd directly.

use std::collections::HashSet;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use reportify::Report;
use reportify::ResultExt as _;

use crate::Architecture;
use crate::ConfigError;

/// A validated, immutable QEMU VM configuration.
#[derive(Clone, Debug)]
#[must_use]
pub struct VmConfig {
    pub(crate) architecture: Architecture,
    pub(crate) qemu_binary: PathBuf,
    pub(crate) virtiofsd_binary: PathBuf,
    pub(crate) system_disk_image: PathBuf,
    pub(crate) system_disk_image_qemu: String,
    pub(crate) data_disk_image: PathBuf,
    pub(crate) data_disk_image_qemu: String,
    pub(crate) data_disk_minimum_size: u64,
    pub(crate) shared_directories: Vec<SharedDirectory>,
    pub(crate) shared_directory_transport: SharedDirectoryTransport,
    pub(crate) direct_boot: DirectBoot,
    pub(crate) runtime_directory: PathBuf,
    pub(crate) control_socket: PathBuf,
    pub(crate) qmp_socket: Option<PathBuf>,
    pub(crate) control_port_name: String,
    pub(crate) memory_mib: u32,
    pub(crate) memory_ballooning: bool,
    pub(crate) vcpu_count: u16,
    pub(crate) acceleration: Acceleration,
    pub(crate) startup_timeout: Duration,
    pub(crate) shutdown_timeout: Duration,
}

impl VmConfig {
    /// Starts a builder with conservative Tascarrel defaults.
    pub fn builder() -> VmConfigBuilder {
        VmConfigBuilder::default()
    }

    /// Returns the guest architecture.
    #[must_use]
    pub const fn architecture(&self) -> Architecture {
        self.architecture
    }

    /// Returns the configured QEMU executable.
    #[must_use]
    pub fn qemu_binary(&self) -> &Path {
        &self.qemu_binary
    }

    /// Returns the configured Linux virtiofsd executable.
    #[must_use]
    pub fn virtiofsd_binary(&self) -> &Path {
        &self.virtiofsd_binary
    }

    /// Returns the immutable raw system filesystem image.
    #[must_use]
    pub fn system_disk_image(&self) -> &Path {
        &self.system_disk_image
    }

    /// Returns the writable persistent data disk image path.
    #[must_use]
    pub fn data_disk_image(&self) -> &Path {
        &self.data_disk_image
    }

    /// Returns the minimum virtual size of the persistent data disk.
    ///
    /// An existing larger image is retained at its current size.
    #[must_use]
    pub const fn data_disk_minimum_size(&self) -> u64 {
        self.data_disk_minimum_size
    }

    /// Returns the host directories exposed to the guest.
    pub fn shared_directories(&self) -> &[SharedDirectory] {
        &self.shared_directories
    }

    /// Returns the currently selected transport for shared directories.
    ///
    /// A newly built configuration contains the host default. Preflight may
    /// replace Linux virtiofs with virtio-9p when virtiofsd is unavailable.
    #[must_use]
    pub const fn shared_directory_transport(&self) -> SharedDirectoryTransport {
        self.shared_directory_transport
    }

    /// Returns the host kernel used for direct Linux boot.
    #[must_use]
    pub fn kernel(&self) -> &Path {
        &self.direct_boot.kernel
    }

    /// Returns the host initrd used for direct Linux boot.
    #[must_use]
    pub fn initrd(&self) -> &Path {
        &self.direct_boot.initrd
    }

    /// Returns the private directory containing transient VM artifacts.
    ///
    /// Known artifacts are removed when the VM finishes. The directory itself
    /// is removed only when empty, so consumer-owned entries are preserved.
    #[must_use]
    pub fn runtime_directory(&self) -> &Path {
        &self.runtime_directory
    }

    /// Returns the host-side virtio-serial Unix socket path.
    #[must_use]
    pub fn control_socket(&self) -> &Path {
        &self.control_socket
    }

    /// Returns the optional private QEMU machine-protocol socket.
    #[must_use]
    pub fn qmp_socket(&self) -> Option<&Path> {
        self.qmp_socket.as_deref()
    }

    /// Returns the guest-visible virtio-serial port name.
    #[must_use]
    pub fn control_port_name(&self) -> &str {
        &self.control_port_name
    }

    /// Returns configured guest memory in MiB.
    #[must_use]
    pub const fn memory_mib(&self) -> u32 {
        self.memory_mib
    }

    /// Returns whether virtio memory ballooning is enabled.
    #[must_use]
    pub const fn memory_ballooning(&self) -> bool {
        self.memory_ballooning
    }

    /// Returns the number of virtual CPUs.
    #[must_use]
    pub const fn vcpu_count(&self) -> u16 {
        self.vcpu_count
    }

    /// Returns the configured acceleration policy.
    #[must_use]
    pub const fn acceleration(&self) -> Acceleration {
        self.acceleration
    }

    /// Returns the maximum time spent waiting for the control channel.
    #[must_use]
    pub const fn startup_timeout(&self) -> Duration {
        self.startup_timeout
    }

    /// Returns the graceful shutdown deadline.
    #[must_use]
    pub const fn shutdown_timeout(&self) -> Duration {
        self.shutdown_timeout
    }
}

/// Builder for [`VmConfig`].
#[derive(Clone, Debug)]
#[must_use]
pub struct VmConfigBuilder {
    architecture: Option<Architecture>,
    qemu_binary: Option<PathBuf>,
    virtiofsd_binary: Option<PathBuf>,
    system_disk_image: Option<PathBuf>,
    data_disk: Option<DataDisk>,
    shared_directories: Vec<SharedDirectory>,
    direct_boot: Option<DirectBoot>,
    runtime_directory: Option<PathBuf>,
    qmp_enabled: bool,
    control_port_name: String,
    memory_mib: u32,
    memory_ballooning: bool,
    vcpu_count: u16,
    acceleration: Acceleration,
    startup_timeout: Duration,
    shutdown_timeout: Duration,
}

impl VmConfigBuilder {
    /// Creates a builder with the same defaults as [`VmConfig::builder`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Selects a guest architecture. The host architecture is used by default.
    pub fn architecture(mut self, architecture: Architecture) -> Self {
        self.architecture = Some(architecture);
        self
    }

    /// Overrides the conventional architecture-specific QEMU executable.
    pub fn qemu_binary(mut self, executable: impl Into<PathBuf>) -> Self {
        self.qemu_binary = Some(executable.into());
        self
    }

    /// Overrides the `virtiofsd` executable used on Linux hosts.
    pub fn virtiofsd_binary(mut self, executable: impl Into<PathBuf>) -> Self {
        self.virtiofsd_binary = Some(executable.into());
        self
    }

    /// Sets the immutable raw system filesystem image.
    pub fn system_disk_image(mut self, path: impl Into<PathBuf>) -> Self {
        self.system_disk_image = Some(path.into());
        self
    }

    /// Sets the managed writable persistent data disk.
    ///
    /// [`crate::Vm::spawn`] creates or grows a sparse raw image to at least
    /// `minimum_size`. An existing larger image is never shrunk.
    pub fn data_disk(mut self, path: impl Into<PathBuf>, minimum_size: u64) -> Self {
        self.data_disk = Some(DataDisk {
            image: path.into(),
            minimum_size,
        });
        self
    }

    /// Exposes one host directory to the guest.
    ///
    /// The directory is identified in the guest by its mount tag. Its path
    /// must be absolute; existence is checked immediately before QEMU starts.
    pub fn shared_directory(mut self, directory: SharedDirectory) -> Self {
        self.shared_directories.push(directory);
        self
    }

    /// Sets the kernel, initrd, and command line used to boot Linux directly.
    pub fn direct_kernel_boot(
        mut self,
        kernel: impl Into<PathBuf>,
        initrd: impl Into<PathBuf>,
        append: impl Into<String>,
    ) -> Self {
        self.direct_boot = Some(DirectBoot {
            kernel: kernel.into(),
            initrd: initrd.into(),
            append: append.into(),
        });
        self
    }

    /// Sets the private directory for transient VM runtime artifacts.
    ///
    /// The path must be absolute. [`crate::Vm::spawn`] creates the directory
    /// with mode `0700` when it is missing. An existing directory must be owned
    /// by the Tascarrel user and have no group or other permission bits.
    /// Tascarrel manages fixed artifact names beneath it, including
    /// `control.sock`, optional `qmp.sock`, and Linux virtiofs sockets. Give
    /// each concurrent VM a distinct runtime directory.
    pub fn runtime_directory(mut self, path: impl Into<PathBuf>) -> Self {
        self.runtime_directory = Some(path.into());
        self
    }

    /// Enables the private QEMU machine-protocol socket used for hotplug.
    ///
    /// The socket is created as `qmp.sock` in [`Self::runtime_directory`]. It
    /// is disabled by default.
    pub fn qmp_enabled(mut self, enabled: bool) -> Self {
        self.qmp_enabled = enabled;
        self
    }

    /// Overrides the guest-visible virtio-serial port name.
    pub fn control_port_name(mut self, name: impl Into<String>) -> Self {
        self.control_port_name = name.into();
        self
    }

    /// Sets guest memory in MiB.
    pub fn memory_mib(mut self, memory_mib: u32) -> Self {
        self.memory_mib = memory_mib;
        self
    }

    /// Enables or disables virtio memory ballooning.
    ///
    /// Ballooning is enabled by default and reports unused guest pages so the
    /// host can reclaim them.
    pub fn memory_ballooning(mut self, enabled: bool) -> Self {
        self.memory_ballooning = enabled;
        self
    }

    /// Sets the number of guest virtual CPUs.
    pub fn vcpu_count(mut self, vcpu_count: u16) -> Self {
        self.vcpu_count = vcpu_count;
        self
    }

    /// Sets the acceleration policy.
    pub fn acceleration(mut self, acceleration: Acceleration) -> Self {
        self.acceleration = acceleration;
        self
    }

    /// Sets the control-channel readiness deadline.
    pub fn startup_timeout(mut self, timeout: Duration) -> Self {
        self.startup_timeout = timeout;
        self
    }

    /// Sets how long graceful shutdown may take before QEMU is killed.
    pub fn shutdown_timeout(mut self, timeout: Duration) -> Self {
        self.shutdown_timeout = timeout;
        self
    }

    /// Validates all values and builds an immutable configuration.
    ///
    /// System-image existence and data-disk preparation are deferred to
    /// [`crate::Vm::spawn`].
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] when a required path is absent, a numeric value
    /// or timeout is zero, the socket path is invalid, or an identifier
    /// violates a QEMU lifecycle constraint.
    #[tracing::instrument(
        name = "tascarrel_vm.config.build",
        level = "debug",
        skip_all,
        fields(
            architecture = ?self.architecture,
            shared_directories = self.shared_directories.len(),
            qmp_enabled = self.qmp_enabled,
            data_disk_minimum_size = self
                .data_disk
                .as_ref()
                .map_or(0, |disk| disk.minimum_size),
            memory_mib = self.memory_mib,
            memory_ballooning = self.memory_ballooning,
            vcpu_count = self.vcpu_count,
            acceleration = ?self.acceleration,
        ),
        err
    )]
    pub fn build(self) -> Result<VmConfig, Report<ConfigError>> {
        let architecture = match self.architecture {
            Some(value) => value,
            None => Architecture::host()
                .map_err(|report| report.escalate(ConfigError::UnsupportedHostArchitecture))?,
        };
        let system_disk_image = self
            .system_disk_image
            .ok_or(ConfigError::MissingSystemDiskImage)
            .report()?;
        if system_disk_image.as_os_str().is_empty() {
            return Err(Report::new(ConfigError::MissingSystemDiskImage));
        }
        let system_disk_image_qemu = system_disk_image
            .to_str()
            .ok_or_else(|| ConfigError::NonUtf8SystemDiskPath(system_disk_image.clone()))
            .report()?
            .to_owned();
        let data_disk = self
            .data_disk
            .ok_or(ConfigError::MissingDataDisk)
            .report()?;
        let data_disk_image = data_disk.image;
        if data_disk_image.as_os_str().is_empty() {
            return Err(Report::new(ConfigError::MissingDataDisk));
        }
        if data_disk.minimum_size == 0 {
            return Err(Report::new(ConfigError::ZeroDataDiskSize));
        }
        if data_disk_image == system_disk_image {
            return Err(Report::new(ConfigError::DataDiskIsSystemImage(
                data_disk_image,
            )));
        }
        let data_disk_image_qemu = data_disk_image
            .to_str()
            .ok_or_else(|| ConfigError::NonUtf8DataDiskPath(data_disk_image.clone()))
            .report()?
            .to_owned();
        let runtime_artifacts =
            validate_runtime_artifacts(self.runtime_directory, self.qmp_enabled)?;
        let shared_directory_transport = SharedDirectoryTransport::host_default();
        let shared_directories = validate_shared_directories(
            self.shared_directories,
            &runtime_artifacts.directory,
            shared_directory_transport,
        )?;
        if self.memory_mib == 0 {
            return Err(Report::new(ConfigError::ZeroMemory));
        }
        if self.vcpu_count == 0 {
            return Err(Report::new(ConfigError::ZeroVcpus));
        }
        if self.startup_timeout.is_zero() {
            return Err(Report::new(ConfigError::ZeroTimeout { name: "startup" }));
        }
        if self.shutdown_timeout.is_zero() {
            return Err(Report::new(ConfigError::ZeroTimeout { name: "shutdown" }));
        }
        if self.control_port_name.is_empty()
            || !self
                .control_port_name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
        {
            return Err(Report::new(ConfigError::InvalidControlPortName(
                self.control_port_name,
            )));
        }
        let direct_boot = validate_direct_boot(self.direct_boot)?;

        Ok(VmConfig {
            architecture,
            qemu_binary: self
                .qemu_binary
                .unwrap_or_else(|| PathBuf::from(architecture.qemu_binary())),
            virtiofsd_binary: self
                .virtiofsd_binary
                .unwrap_or_else(|| PathBuf::from("virtiofsd")),
            system_disk_image,
            system_disk_image_qemu,
            data_disk_image,
            data_disk_image_qemu,
            data_disk_minimum_size: data_disk.minimum_size,
            shared_directories,
            shared_directory_transport,
            direct_boot,
            runtime_directory: runtime_artifacts.directory,
            control_socket: runtime_artifacts.control_socket,
            qmp_socket: runtime_artifacts.qmp_socket,
            control_port_name: self.control_port_name,
            memory_mib: self.memory_mib,
            memory_ballooning: self.memory_ballooning,
            vcpu_count: self.vcpu_count,
            acceleration: self.acceleration,
            startup_timeout: self.startup_timeout,
            shutdown_timeout: self.shutdown_timeout,
        })
    }
}

impl Default for VmConfigBuilder {
    fn default() -> Self {
        Self {
            architecture: None,
            qemu_binary: None,
            virtiofsd_binary: None,
            system_disk_image: None,
            data_disk: None,
            shared_directories: Vec::new(),
            direct_boot: None,
            runtime_directory: None,
            qmp_enabled: false,
            control_port_name: DEFAULT_CONTROL_PORT_NAME.to_owned(),
            memory_mib: DEFAULT_MEMORY_MIB,
            memory_ballooning: true,
            vcpu_count: DEFAULT_VCPU_COUNT,
            acceleration: Acceleration::default(),
            startup_timeout: DEFAULT_STARTUP_TIMEOUT,
            shutdown_timeout: DEFAULT_SHUTDOWN_TIMEOUT,
        }
    }
}

/// A host directory exported to the guest.
///
/// Linux hosts prefer virtiofs, mounted with
/// `mount -t virtiofs source /mnt/source`, and fall back to virtio-9p when
/// virtiofsd is unavailable. On macOS, Tascarrel uses virtio-9p, mounted with
/// `mount -t 9p -o trans=virtio,version=9p2000.L source /mnt/source`.
/// [`VmConfig::shared_directory_transport`] reports the selected transport.
#[derive(Clone, Debug)]
#[must_use]
pub struct SharedDirectory {
    pub(crate) host_path: PathBuf,
    pub(crate) mount_tag: String,
    pub(crate) access: SharedDirectoryAccess,
    pub(crate) socket_path: PathBuf,
}

impl SharedDirectory {
    /// Configures a host directory with an explicit guest access mode.
    pub fn new(
        host_path: impl Into<PathBuf>,
        mount_tag: impl Into<String>,
        access: SharedDirectoryAccess,
    ) -> Self {
        Self {
            host_path: host_path.into(),
            mount_tag: mount_tag.into(),
            access,
            socket_path: PathBuf::new(),
        }
    }

    /// Configures a directory that the guest cannot modify through the share.
    pub fn read_only(host_path: impl Into<PathBuf>, mount_tag: impl Into<String>) -> Self {
        Self::new(host_path, mount_tag, SharedDirectoryAccess::ReadOnly)
    }

    /// Configures a directory that the guest may modify within host permission
    /// constraints.
    pub fn read_write(host_path: impl Into<PathBuf>, mount_tag: impl Into<String>) -> Self {
        Self::new(host_path, mount_tag, SharedDirectoryAccess::ReadWrite)
    }

    /// Returns the exported host directory path.
    #[must_use]
    pub fn host_path(&self) -> &Path {
        &self.host_path
    }

    /// Returns the tag by which the guest identifies the directory.
    #[must_use]
    pub fn mount_tag(&self) -> &str {
        &self.mount_tag
    }

    /// Returns the guest's configured access to the directory.
    #[must_use]
    pub const fn access(&self) -> SharedDirectoryAccess {
        self.access
    }

    /// Returns the derived Linux vhost-user socket path.
    #[must_use]
    pub(crate) fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Returns the PID-file path managed by Linux virtiofsd.
    #[must_use]
    pub(crate) fn pid_file_path(&self) -> PathBuf {
        let mut path = self.socket_path.as_os_str().to_os_string();
        path.push(".pid");
        PathBuf::from(path)
    }
}

/// Guest access granted to a shared host directory.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SharedDirectoryAccess {
    /// The selected backend rejects operations that would modify the export.
    ReadOnly,
    /// The guest may modify entries allowed by the backend's host permissions.
    ReadWrite,
}

/// Host transport used to expose configured shared directories.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SharedDirectoryTransport {
    /// Virtiofs serviced by a namespace- and seccomp-sandboxed virtiofsd.
    Virtiofs,
    /// QEMU's in-process virtio-9p compatibility backend.
    Virtio9p,
}

impl SharedDirectoryTransport {
    const fn host_default() -> Self {
        if cfg!(target_os = "linux") {
            Self::Virtiofs
        } else {
            Self::Virtio9p
        }
    }
}

/// QEMU acceleration policy.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Acceleration {
    /// Prefer the native host accelerator (KVM or HVF), otherwise TCG.
    #[default]
    Auto,
    /// Require Linux KVM hardware acceleration.
    Kvm,
    /// Require Apple's Hypervisor.framework acceleration.
    Hvf,
    /// Use QEMU's portable software translator.
    Tcg,
}

impl Acceleration {
    /// Resolves automatic acceleration against host capabilities.
    #[tracing::instrument(
        name = "tascarrel_vm.acceleration.resolve",
        level = "debug",
        fields(requested = ?self, %architecture),
        ret
    )]
    pub(crate) fn resolve(self, architecture: Architecture) -> Self {
        if self != Self::Auto {
            return self;
        }

        let native = Architecture::host().is_ok_and(|host| host == architecture);
        if cfg!(target_os = "macos") && native {
            Self::Hvf
        } else if native
            && std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open("/dev/kvm")
                .is_ok()
        {
            Self::Kvm
        } else {
            Self::Tcg
        }
    }
}

/// Default guest-visible virtio-serial port name.
pub const DEFAULT_CONTROL_PORT_NAME: &str = "tascarrel-control";
/// Stable virtio block serial used for the persistent workspace data disk.
pub const DATA_DISK_SERIAL: &str = "tascarrel-data";

/// Managed sparse raw data-disk configuration awaiting validation.
#[derive(Clone, Debug)]
struct DataDisk {
    image: PathBuf,
    minimum_size: u64,
}

/// Direct Linux boot artifacts and kernel command line.
#[derive(Clone, Debug)]
pub(crate) struct DirectBoot {
    pub(crate) kernel: PathBuf,
    pub(crate) initrd: PathBuf,
    pub(crate) append: String,
}

const DEFAULT_MEMORY_MIB: u32 = 2048;
const DEFAULT_VCPU_COUNT: u16 = 2;
const DEFAULT_STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);
/// Maximum UTF-8 byte length of the virtiofs device tag field.
const VIRTIOFS_TAG_MAX: usize = 36;
// The smallest common sockaddr_un.sun_path is on macOS (104 bytes including
// NUL).
const PORTABLE_UNIX_SOCKET_PATH_MAX: usize = 103;
const CONTROL_SOCKET_FILE_NAME: &str = "control.sock";
const QMP_SOCKET_FILE_NAME: &str = "qmp.sock";

/// Validated paths managed inside one VM runtime directory.
struct RuntimeArtifacts {
    directory: PathBuf,
    control_socket: PathBuf,
    qmp_socket: Option<PathBuf>,
}

/// Derives the sockets managed inside one runtime directory.
fn validate_runtime_artifacts(
    runtime_directory: Option<PathBuf>,
    qmp_enabled: bool,
) -> Result<RuntimeArtifacts, Report<ConfigError>> {
    let directory = validate_runtime_directory(runtime_directory)?;
    let control_socket = validate_runtime_socket_path(directory.join(CONTROL_SOCKET_FILE_NAME))?;
    let qmp_socket = qmp_enabled
        .then(|| directory.join(QMP_SOCKET_FILE_NAME))
        .map(validate_runtime_socket_path)
        .transpose()?;
    Ok(RuntimeArtifacts {
        directory,
        control_socket,
        qmp_socket,
    })
}

/// Validates and returns the required absolute runtime directory.
fn validate_runtime_directory(
    runtime_directory: Option<PathBuf>,
) -> Result<PathBuf, Report<ConfigError>> {
    let runtime_directory = runtime_directory
        .ok_or(ConfigError::MissingRuntimeDirectory)
        .report()?;
    if runtime_directory.as_os_str().is_empty() {
        return Err(Report::new(ConfigError::MissingRuntimeDirectory));
    }
    if !runtime_directory.is_absolute() {
        return Err(Report::new(ConfigError::RelativeRuntimeDirectory(
            runtime_directory,
        )));
    }
    if runtime_directory.to_str().is_none() {
        return Err(Report::new(ConfigError::NonUtf8RuntimeDirectory(
            runtime_directory,
        )));
    }
    Ok(runtime_directory)
}

/// Enforces the smallest supported host's Unix socket-path limit.
fn validate_runtime_socket_path(path: PathBuf) -> Result<PathBuf, Report<ConfigError>> {
    let length = path.as_os_str().as_bytes().len();
    if length > PORTABLE_UNIX_SOCKET_PATH_MAX {
        return Err(Report::new(ConfigError::SocketPathTooLong {
            path,
            length,
            maximum: PORTABLE_UNIX_SOCKET_PATH_MAX,
        }));
    }
    Ok(path)
}

/// Validates paths and guest-visible identifiers for shared directories.
fn validate_shared_directories(
    mut directories: Vec<SharedDirectory>,
    runtime_directory: &Path,
    transport: SharedDirectoryTransport,
) -> Result<Vec<SharedDirectory>, Report<ConfigError>> {
    let mut mount_tags = HashSet::with_capacity(directories.len());
    for directory in &mut directories {
        if !directory.host_path.is_absolute() {
            return Err(Report::new(ConfigError::RelativeSharedDirectoryPath(
                directory.host_path.clone(),
            )));
        }
        if directory.host_path.to_str().is_none() {
            return Err(Report::new(ConfigError::NonUtf8SharedDirectoryPath(
                directory.host_path.clone(),
            )));
        }
        if directory.mount_tag.is_empty() || directory.mount_tag.contains('\0') {
            return Err(Report::new(ConfigError::InvalidSharedDirectoryMountTag(
                directory.mount_tag.clone(),
            )));
        }
        let mount_tag_length = directory.mount_tag.len();
        if mount_tag_length > VIRTIOFS_TAG_MAX {
            return Err(Report::new(ConfigError::SharedDirectoryMountTagTooLong {
                mount_tag: directory.mount_tag.clone(),
                length: mount_tag_length,
                maximum: VIRTIOFS_TAG_MAX,
            }));
        }
        if transport == SharedDirectoryTransport::Virtiofs {
            directory.socket_path = validate_runtime_socket_path(shared_directory_socket_path(
                runtime_directory,
                mount_tags.len(),
            ))?;
        }
        if !mount_tags.insert(directory.mount_tag.clone()) {
            return Err(Report::new(ConfigError::DuplicateSharedDirectoryMountTag(
                directory.mount_tag.clone(),
            )));
        }
    }
    Ok(directories)
}

/// Derives a private vhost-user socket inside the VM runtime directory.
fn shared_directory_socket_path(runtime_directory: &Path, index: usize) -> PathBuf {
    runtime_directory.join(format!("virtiofs-{index}.sock"))
}

/// Validates and returns the required direct kernel boot configuration.
fn validate_direct_boot(
    direct_boot: Option<DirectBoot>,
) -> Result<DirectBoot, Report<ConfigError>> {
    let direct_boot = direct_boot
        .ok_or(ConfigError::MissingDirectKernelBoot)
        .report()?;
    if direct_boot.append.trim().is_empty() {
        return Err(Report::new(ConfigError::EmptyKernelCommandLine));
    }
    Ok(direct_boot)
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    use super::*;

    fn base_builder() -> VmConfigBuilder {
        VmConfig::builder()
            .architecture(Architecture::X86_64)
            .system_disk_image("/images/tascarrel.erofs")
            .data_disk("/images/tascarrel-data.raw", 1024_u64.pow(4))
            .direct_kernel_boot(
                "/payload/kernel",
                "/payload/initrd",
                "init=/nix/store/system/init",
            )
            .runtime_directory("/run/tascarrel")
    }

    /// Requires a complete managed data-disk specification with nonzero
    /// capacity.
    #[test]
    fn rejects_missing_or_zero_size_data_disk() {
        let missing = VmConfig::builder()
            .architecture(Architecture::X86_64)
            .system_disk_image("/images/tascarrel.erofs")
            .direct_kernel_boot(
                "/payload/kernel",
                "/payload/initrd",
                "init=/nix/store/system/init",
            )
            .runtime_directory("/run/tascarrel")
            .build();
        assert!(matches!(
            missing,
            Err(error) if matches!(error.error(), ConfigError::MissingDataDisk)
        ));

        let zero_size = base_builder()
            .data_disk("/images/tascarrel-data.raw", 0)
            .build();
        assert!(matches!(
            zero_size,
            Err(error) if matches!(error.error(), ConfigError::ZeroDataDiskSize)
        ));
    }

    /// Rejects paths that QEMU's portable option syntax cannot preserve.
    #[test]
    fn rejects_non_utf8_shared_directory_paths() {
        let non_utf8 = base_builder()
            .shared_directory(SharedDirectory::read_only(
                PathBuf::from(OsString::from_vec(vec![b'/', b'h', 0xff])),
                "source",
            ))
            .build();
        assert!(matches!(
            non_utf8,
            Err(error)
                if matches!(error.error(), ConfigError::NonUtf8SharedDirectoryPath(_))
        ));
    }

    /// Rejects mount tags that are unusable or ambiguous inside the guest.
    #[test]
    fn rejects_invalid_or_duplicate_shared_directory_mount_tags() {
        let empty = base_builder()
            .shared_directory(SharedDirectory::read_only("/host/source", ""))
            .build();
        assert!(matches!(
            empty,
            Err(error)
                if matches!(error.error(), ConfigError::InvalidSharedDirectoryMountTag(_))
        ));

        let too_long = base_builder()
            .shared_directory(SharedDirectory::read_only(
                "/host/source",
                "x".repeat(VIRTIOFS_TAG_MAX + 1),
            ))
            .build();
        assert!(matches!(
            too_long,
            Err(error)
                if matches!(
                    error.error(),
                    ConfigError::SharedDirectoryMountTagTooLong { .. }
                )
        ));

        let duplicate = base_builder()
            .shared_directory(SharedDirectory::read_only("/host/source", "source"))
            .shared_directory(SharedDirectory::read_write("/host/output", "source"))
            .build();
        assert!(matches!(
            duplicate,
            Err(error)
                if matches!(
                    error.error(),
                    ConfigError::DuplicateSharedDirectoryMountTag(_)
                )
        ));
    }
}
