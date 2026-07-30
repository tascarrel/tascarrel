//! A small, opinionated QEMU virtual-machine library for Tascarrel.
//!
//! [`Vm`] is the primary interface. A validated [`VmConfig`] describes its
//! architecture, storage, shared host directories, direct kernel boot,
//! resources, and a private runtime directory. Shared directories use managed
//! virtiofsd backends with namespace and seccomp sandboxing on Linux and
//! QEMU's in-process virtio-9p backend on macOS.
//!
//! The crate builds deterministic QEMU invocations, waits for a virtio-serial
//! control channel, and asynchronously owns the QEMU process for its complete
//! lifetime. The system filesystem is attached read-only; mutable guest state
//! belongs on the separate data disk. [`Vm::spawn`] creates that sparse raw
//! disk or grows it without destructive shrinking and returns a [`VmSpawn`]
//! whose [`VmSpawn::take_serial_output`] handle implements Tokio's asynchronous
//! read interface. [`ensure_sparse_raw_disk`] exposes the underlying
//! preparation primitive. A default-on virtio balloon reports unused guest
//! pages so the host can reclaim them. Tascarrel removes known runtime
//! artifacts at shutdown and removes that directory only when it is empty.
//! [`preflight`] reports resolved executable paths and versions before startup;
//! Linux configurations fall back to virtio-9p when virtiofsd is unavailable.
//! [`query_host_usb_devices`] discovers Linux host devices suitable for USB
//! passthrough and reports whether the current process has usbfs access;
//! [`Vm::attach_usb`] and [`Vm::detach_usb`] manage their runtime forwarding.
//!
//! # Configuration Example
//!
//! ```
//! use std::time::Duration;
//! use reportify::Report;
//! use tascarrel_vm::{Architecture, ConfigError, SharedDirectory, VmConfig};
//!
//! fn main() -> Result<(), Report<ConfigError>> {
//!     let config = VmConfig::builder()
//!         .architecture(Architecture::X86_64)
//!         .virtiofsd_binary("/usr/libexec/virtiofsd")
//!         .system_disk_image("/nix/store/tascarrel-system.erofs")
//!         .data_disk("/var/lib/tascarrel/data.raw", 1024_u64.pow(4))
//!         .shared_directory(SharedDirectory::read_only(
//!             "/home/alice/project",
//!             "project-source",
//!         ))
//!         .shared_directory(SharedDirectory::read_write(
//!             "/home/alice/.cache/tascarrel",
//!             "build-cache",
//!         ))
//!         .direct_kernel_boot(
//!             "/var/lib/tascarrel/kernel",
//!             "/var/lib/tascarrel/initrd",
//!             "init=/nix/store/system/init console=ttyS0",
//!         )
//!         .runtime_directory("/run/user/1000/tascarrel/vm")
//!         .startup_timeout(Duration::from_secs(30))
//!         .build()?;
//!
//!     assert_eq!(config.memory_mib(), 2048);
//!     assert!(config.memory_ballooning());
//!     Ok(())
//! }
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]

#[cfg(not(unix))]
compile_error!("tascarrel-vm currently requires a Unix host");

mod architecture;
mod command;
mod config;
mod disk;
mod error;
mod preflight;
mod qmp;
mod usb;
mod vm;

pub use architecture::Architecture;
pub use architecture::ArchitectureParseError;
pub use config::Acceleration;
pub use config::DATA_DISK_SERIAL;
pub use config::DEFAULT_CONTROL_PORT_NAME;
pub use config::SharedDirectory;
pub use config::SharedDirectoryAccess;
pub use config::SharedDirectoryTransport;
pub use config::VmConfig;
pub use config::VmConfigBuilder;
pub use disk::SparseRawDiskError;
pub use disk::SparseRawDiskOutcome;
pub use disk::ensure_sparse_raw_disk;
pub use error::ConfigError;
pub use error::VmError;
pub use preflight::ExecutablePreflightReport;
pub use preflight::PreflightReport;
pub use preflight::preflight;
pub use usb::HostUsbDevice;
pub use usb::USB_FORWARDING_PORT_COUNT;
pub use usb::UsbDiscoveryError;
pub use usb::query_host_usb_devices;
pub use vm::ShutdownOutcome;
pub use vm::Vm;
pub use vm::VmSerialOutput;
pub use vm::VmSpawn;
