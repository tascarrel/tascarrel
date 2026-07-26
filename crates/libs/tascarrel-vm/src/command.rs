//! Internal construction of managed QEMU and virtiofsd invocations.

use std::ffi::OsString;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

use reportify::Report;
use reportify::ResultExt as _;
use serde::Serialize;
use thiserror::Error;

use crate::Acceleration;
use crate::DATA_DISK_SERIAL;
use crate::SharedDirectory;
use crate::SharedDirectoryAccess;
use crate::SharedDirectoryTransport;
use crate::USB_FORWARDING_PORT_COUNT;
use crate::VmConfig;

/// The QEMU program and arguments derived from a validated [`VmConfig`].
#[derive(Clone, Debug, Eq, PartialEq)]
#[must_use]
pub(crate) struct QemuCommand {
    program: PathBuf,
    args: Vec<OsString>,
}

impl QemuCommand {
    /// Returns the QEMU executable path or name.
    #[must_use]
    pub(crate) fn program(&self) -> &Path {
        &self.program
    }

    /// Creates a standard-library command with all generated arguments.
    #[must_use]
    pub(crate) fn to_command(&self) -> Command {
        let mut command = Command::new(&self.program);
        command.args(&self.args);
        command
    }
}

/// A virtiofsd program and arguments for one configured shared directory.
#[derive(Clone, Debug, Eq, PartialEq)]
#[must_use]
pub(crate) struct VirtiofsdCommand {
    program: PathBuf,
    args: Vec<OsString>,
}

impl VirtiofsdCommand {
    /// Returns the virtiofsd executable path or name.
    #[must_use]
    pub(crate) fn program(&self) -> &Path {
        &self.program
    }

    /// Creates a standard-library command with all generated arguments.
    #[must_use]
    pub(crate) fn to_command(&self) -> Command {
        let mut command = Command::new(&self.program);
        command.args(&self.args);
        command
    }
}

impl VmConfig {
    /// Generates the QEMU executable and argument list.
    ///
    /// # Errors
    ///
    /// Returns [`QemuCommandError`] if the typed block-device configuration
    /// cannot be serialized.
    #[tracing::instrument(
        name = "tascarrel_vm.command.qemu",
        level = "debug",
        skip(self),
        fields(
            architecture = %self.architecture,
            acceleration = ?self.acceleration,
            shared_directories = self.shared_directories.len(),
        ),
        err
    )]
    pub(crate) fn qemu_command(&self) -> Result<QemuCommand, Report<QemuCommandError>> {
        let acceleration = self.acceleration.resolve(self.architecture);
        let mut args = Vec::with_capacity(50 + self.shared_directories.len() * 4);
        append_machine(&mut args, self, acceleration);
        append_security_policy(&mut args);
        append_host_interfaces(&mut args, self);
        append_boot(&mut args, self);
        append_storage(&mut args, self)?;
        append_shared_directories(&mut args, self);
        append_control_channel(&mut args, self);

        Ok(QemuCommand {
            program: self.qemu_binary.clone(),
            args,
        })
    }

    /// Generates one virtiofsd invocation per shared directory.
    pub(crate) fn virtiofsd_commands(&self) -> Vec<VirtiofsdCommand> {
        if self.shared_directory_transport != SharedDirectoryTransport::Virtiofs {
            return Vec::new();
        }
        self.shared_directories
            .iter()
            .map(|directory| directory.virtiofsd_command(&self.virtiofsd_binary))
            .collect()
    }
}

impl SharedDirectory {
    /// Builds the Linux backend command for this directory.
    fn virtiofsd_command(&self, program: &Path) -> VirtiofsdCommand {
        let mut args = vec![
            OsString::from("--sandbox"),
            OsString::from("namespace"),
            OsString::from("--seccomp"),
            OsString::from("kill"),
            OsString::from("--socket-path"),
            self.socket_path.as_os_str().to_owned(),
            OsString::from("--shared-dir"),
            self.host_path.as_os_str().to_owned(),
            OsString::from("--log-level"),
            OsString::from("warn"),
        ];
        if self.access == SharedDirectoryAccess::ReadOnly {
            args.push(OsString::from("--readonly"));
        }
        VirtiofsdCommand {
            program: program.to_owned(),
            args,
        }
    }
}

/// Failure while encoding the managed QEMU invocation.
#[derive(Debug, Error)]
#[error("failed to encode the managed QEMU invocation")]
pub(crate) struct QemuCommandError(#[source] serde_json::Error);

#[derive(Serialize)]
#[serde(rename_all = "kebab-case")]
struct BlockDevice<'a> {
    driver: &'static str,
    node_name: &'static str,
    file: BlockDeviceFile<'a>,
    read_only: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    discard: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    detect_zeroes: Option<&'static str>,
}

#[derive(Serialize)]
struct BlockDeviceFile<'a> {
    driver: &'static str,
    filename: &'a str,
}

fn append_machine(args: &mut Vec<OsString>, config: &VmConfig, acceleration: Acceleration) {
    let mut machine = config.architecture.qemu_machine().to_owned();
    if uses_virtiofs(config) {
        machine.push_str(",memory-backend=tascarrel-memory");
    }
    let name = if cfg!(target_os = "macos") {
        "tascarrel"
    } else {
        "tascarrel,process=tascarrel-qemu"
    };
    args.extend([
        OsString::from("-name"),
        OsString::from(name),
        OsString::from("-machine"),
        OsString::from(machine),
        OsString::from("-accel"),
        OsString::from(match acceleration {
            Acceleration::Kvm => "kvm",
            Acceleration::Hvf => "hvf",
            Acceleration::Tcg | Acceleration::Auto => "tcg",
        }),
        OsString::from("-cpu"),
        OsString::from(match acceleration {
            Acceleration::Kvm | Acceleration::Hvf => "host",
            Acceleration::Tcg | Acceleration::Auto => "max",
        }),
        OsString::from("-m"),
        OsString::from(config.memory_mib.to_string()),
        OsString::from("-smp"),
        OsString::from(config.vcpu_count.to_string()),
        OsString::from("-display"),
        OsString::from("none"),
    ]);
    if uses_virtiofs(config) {
        args.extend([
            OsString::from("-object"),
            OsString::from(format!(
                "memory-backend-shm,id=tascarrel-memory,size={}M,share=on",
                config.memory_mib
            )),
        ]);
    }
}

fn append_security_policy(args: &mut Vec<OsString>) {
    if cfg!(target_os = "linux") {
        args.extend([
            OsString::from("-sandbox"),
            OsString::from(
                "on,obsolete=deny,elevateprivileges=deny,spawn=deny,resourcecontrol=deny",
            ),
        ]);
    }
    args.extend([
        OsString::from("-nodefaults"),
        OsString::from("-no-user-config"),
        OsString::from("-nic"),
        OsString::from("none"),
        OsString::from("-no-reboot"),
    ]);
}

fn append_host_interfaces(args: &mut Vec<OsString>, config: &VmConfig) {
    args.extend([
        OsString::from("-serial"),
        OsString::from("stdio"),
        OsString::from("-monitor"),
        OsString::from("none"),
    ]);
    if let Some(qmp_socket) = &config.qmp_socket {
        args.extend([
            OsString::from("-qmp"),
            OsString::from(format!(
                "unix:{},server=on,wait=off",
                escape_qemu_option(qmp_socket.to_string_lossy().as_ref())
            )),
            OsString::from("-device"),
            OsString::from(format!(
                "qemu-xhci,id=tascarrel-xhci,p2={USB_FORWARDING_PORT_COUNT},p3={USB_FORWARDING_PORT_COUNT}"
            )),
        ]);
    }
}

fn append_boot(args: &mut Vec<OsString>, config: &VmConfig) {
    let boot = &config.direct_boot;
    args.extend([
        OsString::from("-kernel"),
        boot.kernel.as_os_str().to_owned(),
        OsString::from("-initrd"),
        boot.initrd.as_os_str().to_owned(),
        OsString::from("-append"),
        OsString::from(&boot.append),
    ]);
}

fn append_storage(
    args: &mut Vec<OsString>,
    config: &VmConfig,
) -> Result<(), Report<QemuCommandError>> {
    let system = serialize_block_device(&BlockDevice {
        driver: "raw",
        node_name: "tascarrel-system",
        file: BlockDeviceFile {
            driver: "file",
            filename: &config.system_disk_image_qemu,
        },
        read_only: true,
        discard: None,
        detect_zeroes: None,
    })?;
    args.extend([
        OsString::from("-blockdev"),
        system,
        OsString::from("-device"),
        OsString::from("virtio-blk-pci,drive=tascarrel-system"),
    ]);

    let data = serialize_block_device(&BlockDevice {
        driver: "raw",
        node_name: "tascarrel-data",
        file: BlockDeviceFile {
            driver: "file",
            filename: &config.data_disk_image_qemu,
        },
        read_only: false,
        discard: Some("unmap"),
        detect_zeroes: Some("unmap"),
    })?;
    args.extend([
        OsString::from("-blockdev"),
        data,
        OsString::from("-device"),
        OsString::from(format!(
            "virtio-blk-pci,drive=tascarrel-data,serial={DATA_DISK_SERIAL}"
        )),
    ]);
    Ok(())
}

fn append_control_channel(args: &mut Vec<OsString>, config: &VmConfig) {
    args.extend([
        OsString::from("-object"),
        OsString::from("rng-random,id=tascarrel-rng,filename=/dev/urandom"),
        OsString::from("-device"),
        OsString::from("virtio-rng-pci,rng=tascarrel-rng"),
        OsString::from("-device"),
        OsString::from("virtio-serial-pci,id=tascarrel-virtio-serial"),
        OsString::from("-chardev"),
        OsString::from(format!(
            "socket,id=tascarrel-control,path={},server=on,wait=off",
            escape_qemu_option(config.control_socket.to_string_lossy().as_ref())
        )),
        OsString::from("-device"),
        OsString::from(format!(
            "virtserialport,chardev=tascarrel-control,name={}",
            config.control_port_name
        )),
    ]);
}

/// Appends host-specific shared-directory devices.
fn append_shared_directories(args: &mut Vec<OsString>, config: &VmConfig) {
    match config.shared_directory_transport {
        SharedDirectoryTransport::Virtiofs => append_virtiofs_directories(args, config),
        SharedDirectoryTransport::Virtio9p => append_9p_directories(args, config),
    }
}

/// Appends Linux vhost-user filesystem devices.
fn append_virtiofs_directories(args: &mut Vec<OsString>, config: &VmConfig) {
    for (index, directory) in config.shared_directories.iter().enumerate() {
        let id = format!("tascarrel-virtiofs-{index}");
        args.extend([
            OsString::from("-chardev"),
            OsString::from(format!(
                "socket,id={id},path={}",
                escape_qemu_option(directory.socket_path.to_string_lossy().as_ref())
            )),
            OsString::from("-device"),
            OsString::from(format!(
                "vhost-user-fs-pci,chardev={id},tag={}",
                escape_qemu_option(&directory.mount_tag)
            )),
        ]);
    }
}

/// Appends QEMU's in-process 9p compatibility devices.
fn append_9p_directories(args: &mut Vec<OsString>, config: &VmConfig) {
    for (index, directory) in config.shared_directories.iter().enumerate() {
        let id = format!("tascarrel-share-{index}");
        let mut fsdev = format!(
            "local,id={id},path={},security_model=none",
            escape_qemu_option(directory.host_path.to_string_lossy().as_ref())
        );
        if directory.access == SharedDirectoryAccess::ReadOnly {
            fsdev.push_str(",readonly=on");
        }
        args.extend([
            OsString::from("-fsdev"),
            OsString::from(fsdev),
            OsString::from("-device"),
            OsString::from(format!(
                "virtio-9p-pci,fsdev={id},mount_tag={}",
                escape_qemu_option(&directory.mount_tag)
            )),
        ]);
    }
}

/// Reports whether QEMU needs shared guest memory for virtiofs.
fn uses_virtiofs(config: &VmConfig) -> bool {
    !config.shared_directories.is_empty()
        && config.shared_directory_transport == SharedDirectoryTransport::Virtiofs
}

fn serialize_block_device(device: &BlockDevice<'_>) -> Result<OsString, Report<QemuCommandError>> {
    serde_json::to_string(device)
        .map(OsString::from)
        .map_err(QemuCommandError)
        .report()
}

fn escape_qemu_option(value: &str) -> String {
    value.replace(',', ",,")
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::ffi::OsString;
    use std::path::Path;

    use super::QemuCommand;
    use crate::Acceleration;
    use crate::Architecture;
    use crate::SharedDirectory;
    use crate::SharedDirectoryTransport;
    use crate::VmConfig;
    use crate::VmConfigBuilder;

    fn base_builder(architecture: Architecture) -> VmConfigBuilder {
        VmConfig::builder()
            .architecture(architecture)
            .system_disk_image("/images/tascarrel.erofs")
            .data_disk("/images/tascarrel-data.raw", 1024_u64.pow(4))
            .direct_kernel_boot(
                "/payload/kernel",
                "/payload/initrd",
                "init=/nix/store/system/init console=ttyS0",
            )
            .runtime_directory("/run/tascarrel")
            .acceleration(Acceleration::Tcg)
    }

    /// Generates a complete `x86_64` command with an immutable system image.
    #[test]
    fn generates_x86_64_qemu_arguments() {
        let config = base_builder(Architecture::X86_64).build().unwrap();
        let command = config.qemu_command().unwrap();
        assert_eq!(command.program(), Path::new("qemu-system-x86_64"));
        assert_option(&command, "-machine", "q35");
        assert_option(&command, "-accel", "tcg");
        assert_option(&command, "-kernel", "/payload/kernel");
        assert_option(&command, "-initrd", "/payload/initrd");
        assert_option(
            &command,
            "-append",
            "init=/nix/store/system/init console=ttyS0",
        );
        assert_sandbox_policy(&command);
        assert_option(&command, "-nic", "none");
        assert_option(&command, "-serial", "stdio");
        assert_no_qemu_networking(&command);
        assert_option_value(&command, "-device", "virtio-rng-pci,rng=tascarrel-rng");
        assert_option_value(
            &command,
            "-object",
            "rng-random,id=tascarrel-rng,filename=/dev/urandom",
        );
        assert!(command.args.iter().any(|argument| {
            argument
                .to_string_lossy()
                .contains("virtserialport,chardev=tascarrel-control,name=tascarrel-control")
        }));
        assert!(command.args.iter().any(|argument| {
            let argument = argument.to_string_lossy();
            argument.contains("\"driver\":\"raw\"")
                && argument.contains("/images/tascarrel-data.raw")
                && argument.contains("\"discard\":\"unmap\"")
                && argument.contains("\"detect-zeroes\":\"unmap\"")
        }));
        assert_option_value(
            &command,
            "-device",
            "virtio-blk-pci,drive=tascarrel-data,serial=tascarrel-data",
        );
        assert!(!command.args.iter().any(|argument| argument == "-fsdev"));
        assert!(command.args.iter().any(|argument| {
            let argument = argument.to_string_lossy();
            argument.contains("\"driver\":\"raw\"")
                && argument.contains("/images/tascarrel.erofs")
                && argument.contains("\"read-only\":true")
        }));
        assert_option_value(&command, "-device", "virtio-blk-pci,drive=tascarrel-system");
    }

    /// Routes the serial console through QEMU stdout for managed streaming.
    #[test]
    fn routes_the_serial_console_to_standard_output() {
        let config = base_builder(Architecture::X86_64).build().unwrap();
        assert_option(&config.qemu_command().unwrap(), "-serial", "stdio");
    }

    /// Generates deterministic virtiofs devices and matching backend commands.
    #[cfg(target_os = "linux")]
    #[test]
    fn exposes_shared_directories_with_the_configured_access() {
        let config = base_builder(Architecture::X86_64)
            .virtiofsd_binary("/usr/libexec/virtiofsd")
            .shared_directory(SharedDirectory::read_only(
                "/host/source,tree",
                "source,tree",
            ))
            .shared_directory(SharedDirectory::read_write("/host/cache", "build-cache"))
            .build()
            .unwrap();
        let command = config.qemu_command().unwrap();

        assert_option(&command, "-machine", "q35,memory-backend=tascarrel-memory");
        assert_option_value(
            &command,
            "-object",
            "memory-backend-shm,id=tascarrel-memory,size=2048M,share=on",
        );
        assert_option_value(
            &command,
            "-chardev",
            "socket,id=tascarrel-virtiofs-0,path=/run/tascarrel/virtiofs-0.sock",
        );
        assert_option_value(
            &command,
            "-device",
            "vhost-user-fs-pci,chardev=tascarrel-virtiofs-0,tag=source,,tree",
        );
        assert_option_value(
            &command,
            "-chardev",
            "socket,id=tascarrel-virtiofs-1,path=/run/tascarrel/virtiofs-1.sock",
        );
        assert_option_value(
            &command,
            "-device",
            "vhost-user-fs-pci,chardev=tascarrel-virtiofs-1,tag=build-cache",
        );

        let backends = config.virtiofsd_commands();
        assert_eq!(backends[0].program(), Path::new("/usr/libexec/virtiofsd"));
        assert_eq!(
            backends[0].args,
            [
                "--sandbox",
                "namespace",
                "--seccomp",
                "kill",
                "--socket-path",
                "/run/tascarrel/virtiofs-0.sock",
                "--shared-dir",
                "/host/source,tree",
                "--log-level",
                "warn",
                "--readonly",
            ]
            .map(OsString::from)
        );
        assert_eq!(
            backends[1].args,
            [
                "--sandbox",
                "namespace",
                "--seccomp",
                "kill",
                "--socket-path",
                "/run/tascarrel/virtiofs-1.sock",
                "--shared-dir",
                "/host/cache",
                "--log-level",
                "warn",
            ]
            .map(OsString::from)
        );
    }

    /// Generates the in-process 9p compatibility backend used on macOS.
    #[test]
    fn exposes_shared_directories_through_the_macos_compatibility_transport() {
        let config = base_builder(Architecture::Aarch64)
            .shared_directory(SharedDirectory::read_only(
                "/host/source,tree",
                "source,tree",
            ))
            .shared_directory(SharedDirectory::read_write("/host/cache", "build-cache"))
            .build()
            .unwrap();
        #[cfg(target_os = "linux")]
        let config = {
            let mut config = config;
            config.shared_directory_transport = SharedDirectoryTransport::Virtio9p;
            config
        };
        assert_eq!(
            config.shared_directory_transport(),
            SharedDirectoryTransport::Virtio9p
        );
        let command = config.qemu_command().unwrap();

        assert_option(&command, "-machine", "virt");
        assert!(
            !command
                .args
                .iter()
                .any(|argument| { argument.to_string_lossy().contains("memory-backend-shm") })
        );
        assert_option_value(
            &command,
            "-fsdev",
            "local,id=tascarrel-share-0,path=/host/source,,tree,security_model=none,readonly=on",
        );
        assert_option_value(
            &command,
            "-device",
            "virtio-9p-pci,fsdev=tascarrel-share-0,mount_tag=source,,tree",
        );
        assert_option_value(
            &command,
            "-fsdev",
            "local,id=tascarrel-share-1,path=/host/cache,security_model=none",
        );
        assert!(config.virtiofsd_commands().is_empty());
    }

    /// Adds a private QMP listener and one explicitly sized xHCI controller.
    #[test]
    fn qmp_hotplug_is_private_and_adds_one_xhci_controller() {
        let config = base_builder(Architecture::X86_64)
            .qmp_enabled(true)
            .build()
            .unwrap();
        let command = config.qemu_command().unwrap();
        assert_option(
            &command,
            "-qmp",
            "unix:/run/tascarrel/qmp.sock,server=on,wait=off",
        );
        assert_option(&command, "-monitor", "none");
        assert_option_value(
            &command,
            "-device",
            "qemu-xhci,id=tascarrel-xhci,p2=15,p3=15",
        );
    }

    /// Generates the architecture-specific `AArch64` machine arguments.
    #[test]
    fn generates_aarch64_qemu_arguments() {
        let config = base_builder(Architecture::Aarch64)
            .memory_mib(4096)
            .vcpu_count(4)
            .build()
            .unwrap();
        let command = config.qemu_command().unwrap();
        assert_eq!(command.program(), Path::new("qemu-system-aarch64"));
        assert_option(&command, "-machine", "virt");
        assert_option(&command, "-cpu", "max");
        assert_option(&command, "-m", "4096");
        assert_option(&command, "-smp", "4");
        assert_sandbox_policy(&command);
        assert_option(&command, "-nic", "none");
        assert_option(&command, "-serial", "stdio");
        assert_no_qemu_networking(&command);
        assert_option_value(&command, "-device", "virtio-rng-pci,rng=tascarrel-rng");
    }

    fn assert_no_qemu_networking(command: &QemuCommand) {
        assert!(
            !command.args.iter().any(|argument| {
                let argument = argument.to_string_lossy();
                argument == "-netdev" || argument.contains("virtio-net")
            }),
            "unexpected QEMU networking argument: {:?}",
            command.args
        );
    }

    #[cfg(target_os = "linux")]
    fn assert_sandbox_policy(command: &QemuCommand) {
        assert_option(
            command,
            "-sandbox",
            "on,obsolete=deny,elevateprivileges=deny,spawn=deny,resourcecontrol=deny",
        );
    }

    #[cfg(not(target_os = "linux"))]
    fn assert_sandbox_policy(command: &QemuCommand) {
        assert!(!command.args.iter().any(|argument| argument == "-sandbox"));
    }

    fn assert_option(command: &QemuCommand, option: &str, expected: &str) {
        let index = command
            .args
            .iter()
            .position(|argument| argument == OsStr::new(option))
            .unwrap_or_else(|| panic!("missing argument {option}"));
        assert_eq!(
            command.args.get(index + 1).map(OsString::as_os_str),
            Some(OsStr::new(expected))
        );
    }

    fn assert_option_value(command: &QemuCommand, option: &str, expected: &str) {
        assert!(
            command
                .args
                .windows(2)
                .any(|pair| { pair[0] == OsStr::new(option) && pair[1] == OsStr::new(expected) }),
            "missing argument pair {option} {expected}"
        );
    }
}
