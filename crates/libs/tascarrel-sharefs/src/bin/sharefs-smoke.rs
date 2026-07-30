//! Kernel-level `ShareFS` smoke test used by the NixOS integration test.

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

use tascarrel_sharefs::MountedShareFileSystem;
use tascarrel_sharefs::ShareFileSystem;
use tascarrel_sharefs::ShareFileSystemMountOptions;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = std::env::args_os().skip(1).map(PathBuf::from);
    let lower = arguments.next().ok_or("missing lower directory")?;
    let state = arguments.next().ok_or("missing state directory")?;
    let mountpoint = arguments.next().ok_or("missing mountpoint")?;
    let btrfs = arguments.next().ok_or("missing btrfs executable")?;
    if arguments.next().is_some() {
        return Err("unexpected extra argument".into());
    }

    fs::create_dir_all(&lower)?;
    fs::create_dir_all(&mountpoint)?;
    fs::write(lower.join("document"), b"base\n")?;
    let mounted = share_result(MountedShareFileSystem::mount(
        &lower,
        &state,
        &mountpoint,
        ShareFileSystemMountOptions {
            uid: 0,
            gid: 0,
            allow_other: false,
        },
    ))?;
    eprintln!("sharefs-smoke: mounted");

    fs::write(mountpoint.join(".document.tmp"), b"proposal\n")?;
    fs::set_permissions(
        mountpoint.join(".document.tmp"),
        fs::Permissions::from_mode(0o640),
    )?;
    fs::rename(
        mountpoint.join(".document.tmp"),
        mountpoint.join("document"),
    )?;
    fs::create_dir(mountpoint.join("pod-directory"))?;
    fs::write(mountpoint.join("pod-directory/new"), b"pod\n")?;
    fs::write(lower.join("host-later"), b"host\n")?;

    if fs::read(mountpoint.join("document"))? != b"proposal\n"
        || fs::read(mountpoint.join("host-later"))? != b"host\n"
        || fs::read(lower.join("document"))? != b"base\n"
    {
        return Err("mounted ShareFS view is inconsistent".into());
    }
    let frozen = share_result(mounted.filesystem().freeze())?;
    eprintln!("sharefs-smoke: frozen");
    if share_result(frozen.snapshot())?.len() != 3 {
        return Err("unexpected ShareFS change count".into());
    }
    let snapshot = state
        .parent()
        .ok_or("ShareFS state has no parent")?
        .join("sharefs-smoke-snapshot");
    run_btrfs(&btrfs, &["subvolume", "snapshot"], &[&state, &snapshot])?;
    let snapshotted = share_result(ShareFileSystem::open(&lower, &snapshot))?;
    if share_result(snapshotted.changes())?.len() != 3
        || share_result(snapshotted.read_file("document"))? != b"proposal\n"
    {
        return Err("Btrfs ShareFS snapshot is inconsistent".into());
    }
    drop(snapshotted);
    run_btrfs(
        &btrfs,
        &["subvolume", "delete", "--commit-after"],
        &[&snapshot],
    )?;
    eprintln!("sharefs-smoke: snapshotted");
    share_result(frozen.clear())?;
    drop(frozen);
    eprintln!("sharefs-smoke: cleared");
    if fs::read(mountpoint.join("document"))? != b"base\n"
        || mountpoint.join("pod-directory").exists()
        || !share_result(mounted.filesystem().changes())?.is_empty()
    {
        return Err("cleared ShareFS revision did not restore the lower view".into());
    }
    eprintln!("sharefs-smoke: unmounting");
    share_result(mounted.unmount())?;
    eprintln!("sharefs-smoke: unmounted");
    Ok(())
}

fn share_result<T>(result: tascarrel_sharefs::ShareFsResult<T>) -> Result<T, std::io::Error> {
    result.map_err(|error| std::io::Error::other(error.to_string()))
}

fn run_btrfs(
    program: &Path,
    arguments: &[&str],
    paths: &[&Path],
) -> Result<(), Box<dyn std::error::Error>> {
    let output = Command::new(program).args(arguments).args(paths).output()?;
    if !output.status.success() {
        return Err(format!(
            "btrfs command failed with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )
        .into());
    }
    Ok(())
}
