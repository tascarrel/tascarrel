use std::fs;
use std::os::unix::fs::MetadataExt as _;
use std::os::unix::fs::PermissionsExt as _;

use tascarrel_sharefs::MountedShareFileSystem;
use tascarrel_sharefs::ShareFileSystemMountOptions;
use tascarrel_sharefs::ShareFsError;

/// Verifies ordinary kernel file operations remain isolated while live lower
/// additions merge into the mounted view.
#[test]
#[ignore = "requires /dev/fuse and a working fusermount3"]
fn kernel_mount_supports_editor_style_changes_and_live_lower_entries() {
    let temporary = tempfile::tempdir().unwrap();
    let lower = temporary.path().join("lower");
    let state = temporary.path().join("state");
    let mountpoint = temporary.path().join("mount");
    fs::create_dir(&lower).unwrap();
    fs::create_dir(&mountpoint).unwrap();
    fs::write(lower.join("document"), b"before\n").unwrap();
    let owner = fs::metadata(&mountpoint).unwrap();

    let mounted = MountedShareFileSystem::mount(
        &lower,
        &state,
        &mountpoint,
        ShareFileSystemMountOptions {
            uid: owner.uid(),
            gid: owner.gid(),
            allow_other: false,
        },
    )
    .unwrap_or_else(|error| {
        if let ShareFsError::Fuse { source, .. } = error.error() {
            panic!("{error:?}: {source:?}");
        }
        panic!("{error:?}");
    });

    fs::write(mountpoint.join(".document.tmp"), b"after\n").unwrap();
    fs::set_permissions(
        mountpoint.join(".document.tmp"),
        fs::Permissions::from_mode(0o640),
    )
    .unwrap();
    fs::rename(
        mountpoint.join(".document.tmp"),
        mountpoint.join("document"),
    )
    .unwrap();
    fs::write(lower.join("from-host"), b"host\n").unwrap();

    assert_eq!(fs::read(mountpoint.join("document")).unwrap(), b"after\n");
    assert_eq!(fs::read(mountpoint.join("from-host")).unwrap(), b"host\n");
    assert_eq!(fs::read(lower.join("document")).unwrap(), b"before\n");
    let changes = mounted.filesystem().changes().unwrap();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].path, std::path::Path::new("document"));

    mounted.unmount().unwrap();
}
