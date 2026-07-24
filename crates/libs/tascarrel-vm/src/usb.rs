//! Physical host USB device discovery.
//!
//! [`query_host_usb_devices`] returns the identifiers needed by
//! [`crate::Vm::attach_usb`] together with display metadata and a snapshot of
//! whether the current process has the permissions required for passthrough.
//! Linux is the first discovery backend; the public model is host-neutral so
//! additional operating systems can implement the same contract later.

#[cfg(target_os = "linux")]
use std::fs;
#[cfg(target_os = "linux")]
use std::fs::OpenOptions;
use std::io;
#[cfg(target_os = "linux")]
use std::os::unix::fs::OpenOptionsExt;
#[cfg(target_os = "linux")]
use std::path::Path;
use std::path::PathBuf;

use reportify::Report;
use thiserror::Error;
#[cfg(target_os = "linux")]
use tracing::debug;

#[cfg(target_os = "linux")]
const LINUX_USB_SYSFS_ROOT: &str = "/sys/bus/usb/devices";
#[cfg(target_os = "linux")]
const LINUX_USBFS_ROOT: &str = "/dev/bus/usb";
#[cfg(target_os = "linux")]
const USB_HUB_CLASS: u8 = 0x09;

/// One physical USB device connected to the host.
///
/// The bus and address identify the device for the current connection and can
/// be passed to [`crate::Vm::attach_usb`]. They may change after unplugging or
/// reconnecting the device. Permission status is likewise a snapshot taken
/// during discovery and does not reserve the device.
#[derive(Clone, Debug, Eq, PartialEq)]
#[must_use]
pub struct HostUsbDevice {
    name: String,
    manufacturer: Option<String>,
    product: Option<String>,
    vendor_id: u16,
    product_id: u16,
    serial_number: Option<String>,
    host_bus: u8,
    host_address: u8,
    has_required_permissions: bool,
}

impl HostUsbDevice {
    /// Returns a human-readable name suitable for displaying to the user.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the manufacturer reported by the device, when available.
    #[must_use]
    pub fn manufacturer(&self) -> Option<&str> {
        self.manufacturer.as_deref()
    }

    /// Returns the product name reported by the device, when available.
    #[must_use]
    pub fn product(&self) -> Option<&str> {
        self.product.as_deref()
    }

    /// Returns the USB vendor identifier.
    #[must_use]
    pub const fn vendor_id(&self) -> u16 {
        self.vendor_id
    }

    /// Returns the USB product identifier.
    #[must_use]
    pub const fn product_id(&self) -> u16 {
        self.product_id
    }

    /// Returns the serial number reported by the device, when available.
    #[must_use]
    pub fn serial_number(&self) -> Option<&str> {
        self.serial_number.as_deref()
    }

    /// Returns the host USB bus number used by QEMU passthrough.
    #[must_use]
    pub const fn host_bus(&self) -> u8 {
        self.host_bus
    }

    /// Returns the host USB address used by QEMU passthrough.
    #[must_use]
    pub const fn host_address(&self) -> u8 {
        self.host_address
    }

    /// Reports whether the current process can access the device for
    /// passthrough.
    #[must_use]
    pub const fn has_required_permissions(&self) -> bool {
        self.has_required_permissions
    }
}

/// Number of deterministic xHCI ports available for USB forwarding.
///
/// Managed VM commands configure this number of USB 2 and USB 3 ports so
/// callers can allocate stable port numbers from
/// `1..=USB_FORWARDING_PORT_COUNT`.
pub const USB_FORWARDING_PORT_COUNT: u8 = 15;

/// Queries the physical USB devices that can be passed through to a VM.
///
/// USB hubs are omitted because QEMU cannot pass them through. On Linux, the
/// permission flag reports whether the current process can open the device's
/// usbfs node for reading and writing. Other hosts currently return
/// [`UsbDiscoveryError::UnsupportedPlatform`].
///
/// # Errors
///
/// Returns an error when discovery is unsupported on the host, sysfs cannot be
/// enumerated, an attribute cannot be read, or a numeric attribute is invalid.
#[tracing::instrument(
    name = "tascarrel_vm.usb.query_host_devices",
    level = "debug",
    fields(
        platform = std::env::consts::OS,
        device_count = tracing::field::Empty,
    ),
    err
)]
pub fn query_host_usb_devices() -> Result<Vec<HostUsbDevice>, Report<UsbDiscoveryError>> {
    #[cfg(not(target_os = "linux"))]
    return Err(Report::new(UsbDiscoveryError::UnsupportedPlatform));

    #[cfg(target_os = "linux")]
    {
        let devices = query_linux_host_usb_devices_at(
            Path::new(LINUX_USB_SYSFS_ROOT),
            Path::new(LINUX_USBFS_ROOT),
        )?;
        tracing::Span::current().record("device_count", devices.len());
        Ok(devices)
    }
}

/// An error while discovering physical host USB devices.
#[derive(Debug, Error)]
pub enum UsbDiscoveryError {
    /// The current host has no discovery backend yet.
    #[error("host USB discovery is not supported on this platform")]
    UnsupportedPlatform,
    /// A host discovery filesystem operation failed.
    #[error("failed to {operation} host USB discovery path {path}: {source}")]
    Io {
        /// The operation that failed.
        operation: &'static str,
        /// The host path involved in the operation.
        path: PathBuf,
        /// The underlying host filesystem error.
        #[source]
        source: io::Error,
    },
    /// A numeric USB attribute contained an invalid value.
    #[error("USB attribute {path} contains invalid {kind} value {value:?}")]
    InvalidAttribute {
        /// The sysfs attribute path.
        path: PathBuf,
        /// The expected value kind.
        kind: &'static str,
        /// The invalid trimmed value.
        value: String,
    },
}

/// Enumerates Linux USB devices below configurable roots.
#[cfg(target_os = "linux")]
fn query_linux_host_usb_devices_at(
    sysfs_root: &Path,
    usbfs_root: &Path,
) -> Result<Vec<HostUsbDevice>, Report<UsbDiscoveryError>> {
    let entries =
        fs::read_dir(sysfs_root).map_err(|source| usb_io_error("enumerate", sysfs_root, source))?;
    let mut devices = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| usb_io_error("enumerate", sysfs_root, source))?;
        let sysfs_name = entry.file_name();
        let sysfs_name = sysfs_name.to_string_lossy();
        if sysfs_name.starts_with("usb") || sysfs_name.contains(':') {
            continue;
        }

        let path = entry.path();
        if read_hex_u8(&path.join("bDeviceClass"))? == Some(USB_HUB_CLASS) {
            continue;
        }
        let Some(vendor_id) = read_hex_u16(&path.join("idVendor"))? else {
            continue;
        };
        let Some(product_id) = read_hex_u16(&path.join("idProduct"))? else {
            continue;
        };
        let Some(host_bus) = read_decimal_u8(&path.join("busnum"))? else {
            continue;
        };
        let Some(host_address) = read_decimal_u8(&path.join("devnum"))? else {
            continue;
        };
        let manufacturer = read_usb_string(&path.join("manufacturer"))?;
        let product = read_usb_string(&path.join("product"))?;
        let serial_number = read_usb_string(&path.join("serial"))?;
        let name = usb_display_name(
            manufacturer.as_deref(),
            product.as_deref(),
            vendor_id,
            product_id,
        );
        let device_node = usbfs_root.join(format!("{host_bus:03}/{host_address:03}"));
        devices.push(HostUsbDevice {
            name,
            manufacturer,
            product,
            vendor_id,
            product_id,
            serial_number,
            host_bus,
            host_address,
            has_required_permissions: has_linux_usbfs_permissions(&device_node),
        });
    }
    devices.sort_by_key(|device| (device.host_bus, device.host_address));
    Ok(devices)
}

/// Produces a concise device name with numeric identifiers as the fallback.
#[cfg(target_os = "linux")]
fn usb_display_name(
    manufacturer: Option<&str>,
    product: Option<&str>,
    vendor_id: u16,
    product_id: u16,
) -> String {
    match (manufacturer, product) {
        (Some(manufacturer), Some(product)) if product_starts_with(product, manufacturer) => {
            product.to_owned()
        }
        (Some(manufacturer), Some(product)) => format!("{manufacturer} {product}"),
        (Some(manufacturer), None) => manufacturer.to_owned(),
        (None, Some(product)) => product.to_owned(),
        (None, None) => format!("USB device {vendor_id:04x}:{product_id:04x}"),
    }
}

/// Detects a redundant manufacturer prefix without assuming valid ASCII.
#[cfg(target_os = "linux")]
fn product_starts_with(product: &str, manufacturer: &str) -> bool {
    product.starts_with(manufacturer)
        || product
            .get(..manufacturer.len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(manufacturer))
}

/// Reads and sanitizes a device-controlled USB string attribute.
#[cfg(target_os = "linux")]
fn read_usb_string(path: &Path) -> Result<Option<String>, Report<UsbDiscoveryError>> {
    let Some(value) = read_trimmed(path)? else {
        return Ok(None);
    };
    let sanitized = value
        .split(char::is_control)
        .flat_map(str::split_whitespace)
        .collect::<Vec<_>>()
        .join(" ");
    Ok((!sanitized.is_empty()).then_some(sanitized))
}

/// Reads a trimmed sysfs attribute, treating disappearance as absence.
#[cfg(target_os = "linux")]
fn read_trimmed(path: &Path) -> Result<Option<String>, Report<UsbDiscoveryError>> {
    match fs::read_to_string(path) {
        Ok(value) => {
            let value = value.trim().to_owned();
            Ok((!value.is_empty()).then_some(value))
        }
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(usb_io_error("read", path, source)),
    }
}

/// Reads an optional hexadecimal 16-bit sysfs attribute.
#[cfg(target_os = "linux")]
fn read_hex_u16(path: &Path) -> Result<Option<u16>, Report<UsbDiscoveryError>> {
    parse_optional_attribute(path, "hexadecimal u16", |value| {
        u16::from_str_radix(value, 16)
    })
}

/// Reads an optional hexadecimal 8-bit sysfs attribute.
#[cfg(target_os = "linux")]
fn read_hex_u8(path: &Path) -> Result<Option<u8>, Report<UsbDiscoveryError>> {
    parse_optional_attribute(path, "hexadecimal u8", |value| {
        u8::from_str_radix(value, 16)
    })
}

/// Reads an optional decimal 8-bit sysfs attribute.
#[cfg(target_os = "linux")]
fn read_decimal_u8(path: &Path) -> Result<Option<u8>, Report<UsbDiscoveryError>> {
    parse_optional_attribute(path, "decimal u8", str::parse)
}

/// Parses one optional numeric sysfs attribute into its requested type.
#[cfg(target_os = "linux")]
fn parse_optional_attribute<T, E>(
    path: &Path,
    kind: &'static str,
    parse: impl FnOnce(&str) -> Result<T, E>,
) -> Result<Option<T>, Report<UsbDiscoveryError>> {
    let Some(value) = read_trimmed(path)? else {
        return Ok(None);
    };
    parse(&value).map(Some).map_err(|_| {
        Report::new(UsbDiscoveryError::InvalidAttribute {
            path: path.to_owned(),
            kind,
            value,
        })
    })
}

/// Checks the same read-write device-node access required by QEMU's usbfs
/// backend.
#[cfg(target_os = "linux")]
fn has_linux_usbfs_permissions(path: &Path) -> bool {
    match OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(nix::libc::O_CLOEXEC | nix::libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(file) => {
            drop(file);
            true
        }
        Err(error) => {
            debug!(device_node = %path.display(), %error, "USB device lacks passthrough access");
            false
        }
    }
}

/// Wraps a host filesystem failure in the public discovery error contract.
#[cfg(target_os = "linux")]
fn usb_io_error(
    operation: &'static str,
    path: &Path,
    source: io::Error,
) -> Report<UsbDiscoveryError> {
    Report::new(UsbDiscoveryError::Io {
        operation,
        path: path.to_owned(),
        source,
    })
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use tempfile::tempdir;

    use super::*;

    /// Discovery exposes attachment identifiers, safe names, and access status
    /// while omitting hubs.
    #[test]
    fn discovers_linux_usb_devices_for_passthrough() {
        let temporary = tempdir().unwrap();
        let sysfs = temporary.path().join("sysfs");
        let usbfs = temporary.path().join("usbfs");
        fs::create_dir(&sysfs).unwrap();

        add_device(&sysfs, "2-1", "1366", "0105", "00", 2, 5);
        fs::write(sysfs.join("2-1/manufacturer"), "SEGGER\n").unwrap();
        fs::write(sysfs.join("2-1/product"), "J-Link\u{1b}[31m\n").unwrap();
        fs::write(sysfs.join("2-1/serial"), "PROBE-1\n").unwrap();
        fs::create_dir_all(usbfs.join("002")).unwrap();
        fs::write(usbfs.join("002/005"), []).unwrap();

        add_device(&sysfs, "1-3", "1234", "abcd", "00", 1, 4);
        add_device(&sysfs, "1-4", "1234", "0001", "09", 1, 6);
        fs::create_dir(sysfs.join("1-3:1.0")).unwrap();
        fs::create_dir(sysfs.join("usb1")).unwrap();

        let devices = query_linux_host_usb_devices_at(&sysfs, &usbfs).unwrap();

        assert_eq!(devices.len(), 2);
        assert_eq!(devices[0].name(), "USB device 1234:abcd");
        assert_eq!((devices[0].host_bus(), devices[0].host_address()), (1, 4));
        assert_eq!(
            (devices[0].vendor_id(), devices[0].product_id()),
            (0x1234, 0xabcd)
        );
        assert!(!devices[0].has_required_permissions());
        assert_eq!(devices[1].name(), "SEGGER J-Link [31m");
        assert_eq!(devices[1].manufacturer(), Some("SEGGER"));
        assert_eq!(devices[1].product(), Some("J-Link [31m"));
        assert_eq!(devices[1].serial_number(), Some("PROBE-1"));
        assert!(devices[1].has_required_permissions());
    }

    /// Creates one synthetic Linux sysfs USB device.
    fn add_device(
        root: &Path,
        name: &str,
        vendor_id: &str,
        product_id: &str,
        class: &str,
        bus: u8,
        address: u8,
    ) {
        let device = root.join(name);
        fs::create_dir(&device).unwrap();
        fs::write(device.join("idVendor"), format!("{vendor_id}\n")).unwrap();
        fs::write(device.join("idProduct"), format!("{product_id}\n")).unwrap();
        fs::write(device.join("bDeviceClass"), format!("{class}\n")).unwrap();
        fs::write(device.join("busnum"), format!("{bus}\n")).unwrap();
        fs::write(device.join("devnum"), format!("{address}\n")).unwrap();
    }
}
