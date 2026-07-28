//! Guest USB discovery, node staging, and pod device synchronization.

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use nix::sys::stat::Mode;
use nix::sys::stat::SFlag;
use nix::sys::stat::makedev;
use nix::sys::stat::mknod;
use tracing::info;
use tracing::warn;

use crate::runtime::pod::POD_DEVICE_SOURCE_ROOT;
use crate::runtime::pod::PodDevice;
use crate::runtime::pod::PodDeviceKind;
use crate::services::pods::PodService;

/// Discovers guest-kernel nodes for passed-through USB devices and mirrors
/// them into every running and future pod.
#[derive(Debug)]
#[allow(clippy::struct_field_names)] // Each root names a distinct device namespace.
pub struct UsbGuest {
    usb_root: PathBuf,
    char_root: PathBuf,
    block_root: PathBuf,
    source_root: PathBuf,
}

impl UsbGuest {
    /// Creates the empty curated source before durable pod recovery mounts it.
    ///
    /// # Errors
    ///
    /// Returns an error if the guest-owned devtmpfs directory cannot be
    /// created safely.
    pub fn prepare_source() -> Result<()> {
        reset_source_root(Path::new(POD_DEVICE_SOURCE_ROOT))
    }

    /// Creates a Linux sysfs USB reconciler and its curated device source.
    ///
    /// # Errors
    ///
    /// Returns an error if the guest-owned devtmpfs directory cannot be
    /// created safely.
    pub fn new() -> Result<Self> {
        let guest = Self {
            usb_root: PathBuf::from("/sys/bus/usb/devices"),
            char_root: PathBuf::from("/sys/dev/char"),
            block_root: PathBuf::from("/sys/dev/block"),
            source_root: PathBuf::from(POD_DEVICE_SOURCE_ROOT),
        };
        guest.reset_source_root()?;
        Ok(guest)
    }

    /// Reconciles until canceled. Discovery failures leave the last known
    /// fail-closed pod policy in place and are retried.
    pub async fn run(self, pods: PodService) {
        let mut interval = tokio::time::interval(Duration::from_millis(500));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut applied = Vec::new();
        loop {
            interval.tick().await;
            let devices = match self.discover() {
                Ok(devices) => devices,
                Err(error) => {
                    warn!(%error, "could not discover guest USB device nodes");
                    continue;
                }
            };
            if devices == applied {
                continue;
            }
            if let Err(error) = self.stage(&devices) {
                warn!(%error, "could not stage guest USB device nodes");
                continue;
            }
            match pods.replace_devices(devices.clone()).await {
                Ok(()) => {
                    info!(
                        nodes = devices.len(),
                        "synchronized workspace USB devices into pods"
                    );
                    applied = devices;
                }
                Err(error) => warn!(%error, "could not synchronize USB devices into pods"),
            }
        }
    }

    fn discover(&self) -> Result<Vec<PodDevice>> {
        let physical = physical_devices(&self.usb_root)?;
        let char_devices = sysfs_devices(&self.char_root, PodDeviceKind::Char)?;
        let block_devices = sysfs_devices(&self.block_root, PodDeviceKind::Block)?;
        let mut devices = BTreeSet::new();
        for physical in &physical {
            for node in char_devices
                .iter()
                .chain(&block_devices)
                .filter(|node| node.sysfs_path.starts_with(&physical.path))
                .filter(|node| !node.devname.starts_with("bus/usb/"))
            {
                insert_path(&mut devices, node)?;
            }
            if let (Some((major, minor)), Some(bus), Some(address)) =
                (physical.dev, physical.bus, physical.address)
            {
                let raw = SysfsDevice {
                    sysfs_path: physical.path.clone(),
                    devname: format!("bus/usb/{bus:03}/{address:03}"),
                    kind: PodDeviceKind::Char,
                    major,
                    minor,
                };
                insert_path(&mut devices, &raw)?;
            }
        }
        Ok(devices.into_iter().collect())
    }

    fn stage(&self, devices: &[PodDevice]) -> Result<()> {
        self.reset_source_root()?;
        let mut sources = std::collections::BTreeMap::new();
        for device in devices {
            let identity = (device.kind(), device.major(), device.minor());
            if let Some(previous) = sources.insert(device.source().to_owned(), identity)
                && previous != identity
            {
                anyhow::bail!(
                    "device source {} has conflicting identities",
                    device.source().display()
                );
            }
        }
        for (source, (kind, major, minor)) in sources {
            let relative = source
                .strip_prefix("/dev")
                .with_context(|| format!("device source escaped /dev: {}", source.display()))?;
            let target = self.source_root.join(relative);
            let parent = target
                .parent()
                .context("staged device path has no parent")?;
            fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
            let file_type = match kind {
                PodDeviceKind::Char => SFlag::S_IFCHR,
                PodDeviceKind::Block => SFlag::S_IFBLK,
            };
            mknod(
                &target,
                file_type,
                Mode::from_bits_truncate(0o666),
                makedev(u64::from(major), u64::from(minor)),
            )
            .with_context(|| format!("create staged device {}", target.display()))?;
            fs::set_permissions(&target, fs::Permissions::from_mode(0o666))
                .with_context(|| format!("set permissions on {}", target.display()))?;
        }
        Ok(())
    }

    fn reset_source_root(&self) -> Result<()> {
        reset_source_root(&self.source_root)
    }
}

fn reset_source_root(source_root: &Path) -> Result<()> {
    match fs::symlink_metadata(source_root) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            for entry in fs::read_dir(source_root)
                .with_context(|| format!("read {}", source_root.display()))?
            {
                let path = entry?.path();
                let metadata = fs::symlink_metadata(&path)
                    .with_context(|| format!("inspect {}", path.display()))?;
                if metadata.is_dir() && !metadata.file_type().is_symlink() {
                    fs::remove_dir_all(&path)
                        .with_context(|| format!("remove {}", path.display()))?;
                } else {
                    fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
                }
            }
        }
        Ok(_) => anyhow::bail!(
            "curated device source is not a real directory: {}",
            source_root.display()
        ),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir(source_root)
                .with_context(|| format!("create {}", source_root.display()))?;
        }
        Err(error) => {
            return Err(error).with_context(|| format!("inspect {}", source_root.display()));
        }
    }
    Ok(())
}

#[derive(Debug)]
struct PhysicalUsb {
    path: PathBuf,
    bus: Option<u8>,
    address: Option<u8>,
    dev: Option<(u32, u32)>,
}

#[derive(Debug)]
struct SysfsDevice {
    sysfs_path: PathBuf,
    devname: String,
    kind: PodDeviceKind,
    major: u32,
    minor: u32,
}

fn physical_devices(root: &Path) -> Result<Vec<PhysicalUsb>> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).with_context(|| format!("read {}", root.display())),
    };
    let mut devices = Vec::new();
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with("usb") || name.contains(':') {
            continue;
        }
        let entry_path = entry.path();
        let Some(_) = read_hex_u16(&entry_path.join("idVendor"))? else {
            continue;
        };
        let Some(_) = read_hex_u16(&entry_path.join("idProduct"))? else {
            continue;
        };
        devices.push(PhysicalUsb {
            path: fs::canonicalize(&entry_path)
                .with_context(|| format!("resolve {}", entry_path.display()))?,
            bus: read_decimal_u8(&entry_path.join("busnum"))?,
            address: read_decimal_u8(&entry_path.join("devnum"))?,
            dev: read_device_number(&entry_path.join("dev"))?,
        });
    }
    Ok(devices)
}

fn sysfs_devices(root: &Path, kind: PodDeviceKind) -> Result<Vec<SysfsDevice>> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).with_context(|| format!("read {}", root.display())),
    };
    let mut devices = Vec::new();
    for entry in entries {
        let entry = entry?;
        let Some((major, minor)) = parse_device_number(&entry.file_name().to_string_lossy()) else {
            continue;
        };
        let path = entry.path();
        let sysfs_path = match fs::canonicalize(&path) {
            Ok(path) => path,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error).with_context(|| format!("resolve {}", path.display())),
        };
        let Some(devname) = read_uevent_devname(&path.join("uevent"))? else {
            continue;
        };
        if !safe_devname(&devname) {
            warn!(%devname, "ignoring unsafe sysfs DEVNAME");
            continue;
        }
        devices.push(SysfsDevice {
            sysfs_path,
            devname,
            kind,
            major,
            minor,
        });
    }
    Ok(devices)
}

fn insert_path(devices: &mut BTreeSet<PodDevice>, node: &SysfsDevice) -> Result<()> {
    let original = Path::new("/dev").join(&node.devname);
    devices.insert(PodDevice::new(
        &original, node.kind, node.major, node.minor,
    )?);
    Ok(())
}

fn safe_devname(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('/')
        && Path::new(value)
            .components()
            .all(|component| matches!(component, std::path::Component::Normal(_)))
}

fn read_uevent_devname(path: &Path) -> Result<Option<String>> {
    let Some(contents) = read_trimmed(path)? else {
        return Ok(None);
    };
    Ok(contents
        .lines()
        .find_map(|line| line.strip_prefix("DEVNAME=").map(str::to_owned)))
}

fn read_trimmed(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(value) => {
            let value = value.trim().to_owned();
            Ok((!value.is_empty()).then_some(value))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
    }
}

fn read_hex_u16(path: &Path) -> Result<Option<u16>> {
    read_trimmed(path)?
        .map(|value| {
            u16::from_str_radix(&value, 16).with_context(|| format!("parse {}", path.display()))
        })
        .transpose()
}

fn read_decimal_u8(path: &Path) -> Result<Option<u8>> {
    read_trimmed(path)?
        .map(|value| {
            value
                .parse::<u8>()
                .with_context(|| format!("parse {}", path.display()))
        })
        .transpose()
}

fn read_device_number(path: &Path) -> Result<Option<(u32, u32)>> {
    Ok(read_trimmed(path)?.and_then(|value| parse_device_number(&value)))
}

fn parse_device_number(value: &str) -> Option<(u32, u32)> {
    let (major, minor) = value.split_once(':')?;
    Some((major.parse().ok()?, minor.parse().ok()?))
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use tempfile::tempdir;

    use super::*;

    /// Verifies USB discovery exposes both driver-created and raw usbfs nodes.
    #[test]
    fn discovers_driver_and_raw_nodes() {
        let temporary = tempdir().unwrap();
        let devices = temporary.path().join("devices");
        let physical = devices.join("1-1");
        let tty = physical.join("1-1:1.0/tty/ttyACM0");
        fs::create_dir_all(&tty).unwrap();
        fs::write(physical.join("idVendor"), "2341\n").unwrap();
        fs::write(physical.join("idProduct"), "0043\n").unwrap();
        fs::write(physical.join("serial"), "BOARD\n").unwrap();
        fs::write(physical.join("busnum"), "1\n").unwrap();
        fs::write(physical.join("devnum"), "2\n").unwrap();
        fs::write(physical.join("dev"), "189:1\n").unwrap();
        fs::write(tty.join("uevent"), "MAJOR=166\nMINOR=0\nDEVNAME=ttyACM0\n").unwrap();
        let usb_root = temporary.path().join("usb");
        let char_root = temporary.path().join("char");
        let block_root = temporary.path().join("block");
        fs::create_dir(&usb_root).unwrap();
        fs::create_dir(&char_root).unwrap();
        fs::create_dir(&block_root).unwrap();
        symlink(&physical, usb_root.join("1-1")).unwrap();
        symlink(&tty, char_root.join("166:0")).unwrap();

        let guest = UsbGuest {
            usb_root,
            char_root,
            block_root,
            source_root: temporary.path().join("source"),
        };
        let exposed = guest.discover().unwrap();
        let paths = exposed
            .iter()
            .map(|device| device.path().to_owned())
            .collect::<BTreeSet<_>>();
        assert!(paths.contains(Path::new("/dev/ttyACM0")));
        assert!(paths.contains(Path::new("/dev/bus/usb/001/002")));
    }

    /// Verifies device names cannot escape the staged `/dev` hierarchy.
    #[test]
    fn rejects_escaping_devnames() {
        for value in ["", "/dev/null", "../null", "foo/../bar"] {
            assert!(!safe_devname(value), "accepted {value:?}");
        }
        assert!(safe_devname("input/event0"));
    }
}
