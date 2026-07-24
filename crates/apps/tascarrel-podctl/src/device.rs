//! Privileged device-node helpers invoked by guestd.
//!
//! This module constrains link creation and removal to pod-private paths below
//! `/dev` and staged host device nodes below `/run/tascarrel/host-dev`.

use std::fs;
use std::fs::DirBuilder;
use std::io;
use std::os::unix::fs::DirBuilderExt as _;
use std::os::unix::fs::symlink;
use std::path::Path;
use std::path::PathBuf;

use reportify::ErrorExt as _;
use reportify::ResultExt as _;

use crate::error::PodctlError;
use crate::error::PodctlResult;

/// Creates one validated pod-private link to a staged device node.
pub(crate) fn create_device_link(path: &Path, source: &Path) -> PodctlResult<()> {
    validate_normal_child(path, Path::new("/dev"), "device path")?;
    validate_normal_child(
        source,
        Path::new("/run/tascarrel/host-dev"),
        "device source",
    )?;
    ensure_device_parent(path)?;
    remove_device_node(path)?;
    symlink(source, path).escalate(PodctlError::DeviceIo {
        action: "create the device link",
    })
}

/// Removes one validated pod-private device node or link.
pub(crate) fn remove_device_node(path: &Path) -> PodctlResult<()> {
    validate_normal_child(path, Path::new("/dev"), "device path")?;
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => Err(PodctlError::DevicePathIsDirectory.report()),
        Ok(_) => fs::remove_file(path).escalate(PodctlError::DeviceIo {
            action: "remove the device node",
        }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.escalate(PodctlError::DeviceIo {
            action: "inspect the device node",
        })),
    }
}

/// Requires a path composed only of normal components below a fixed parent.
fn validate_normal_child(path: &Path, parent: &Path, label: &'static str) -> PodctlResult<()> {
    let relative = path
        .strip_prefix(parent)
        .map_err(|_| PodctlError::InvalidDevicePath { kind: label }.report())?;
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err(PodctlError::InvalidDevicePath { kind: label }.report());
    }
    Ok(())
}

/// Creates missing real directory components below `/dev`.
fn ensure_device_parent(path: &Path) -> PodctlResult<()> {
    let parent = path
        .parent()
        .ok_or_else(|| PodctlError::InvalidDeviceParent.report())?;
    let relative = parent
        .strip_prefix("/dev")
        .map_err(|_| PodctlError::InvalidDeviceParent.report())?;
    let mut current = PathBuf::from("/dev");
    for component in relative.components() {
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Ok(_) => {
                return Err(PodctlError::UnsafeDeviceParent.report());
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                DirBuilder::new()
                    .mode(0o755)
                    .create(&current)
                    .escalate(PodctlError::DeviceIo {
                        action: "create a device-link parent",
                    })?;
            }
            Err(error) => {
                return Err(error.escalate(PodctlError::DeviceIo {
                    action: "inspect a device-link parent",
                }));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::validate_normal_child;

    /// Verifies internal device operations cannot escape their guestd-owned
    /// source and pod-private destination trees.
    #[test]
    fn device_helpers_accept_only_normal_scoped_paths() {
        validate_normal_child(Path::new("/dev/ttyACM0"), Path::new("/dev"), "device").unwrap();
        validate_normal_child(
            Path::new("/run/tascarrel/host-dev/bus/usb/001/002"),
            Path::new("/run/tascarrel/host-dev"),
            "source",
        )
        .unwrap();
        for path in ["/dev", "/etc/passwd", "/dev/../etc/passwd"] {
            assert!(validate_normal_child(Path::new(path), Path::new("/dev"), "device").is_err());
        }
    }
}
