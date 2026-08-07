use std::ffi::OsString;
use std::fs;
use std::io::Read as _;
use std::os::unix::ffi::OsStringExt as _;
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::fs::symlink;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use tascarrel_sharefs::ContentDigest;
use tascarrel_sharefs::EntryKind;
use tascarrel_sharefs::FileWriteOutcome;
use tascarrel_sharefs::ShareChange;
use tascarrel_sharefs::ShareFileSystem;
use tascarrel_sharefs::ShareFsError;
use tempfile::TempDir;

struct Fixture {
    temporary: TempDir,
    lower: PathBuf,
    state: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temporary = tempfile::tempdir().unwrap();
        let lower = temporary.path().join("lower");
        let state = temporary.path().join("state");
        fs::create_dir(&lower).unwrap();
        Self {
            temporary,
            lower,
            state,
        }
    }

    fn open(&self) -> ShareFileSystem {
        ShareFileSystem::open(&self.lower, &self.state).unwrap()
    }
}

fn entry_names(filesystem: &ShareFileSystem, path: impl AsRef<Path>) -> Vec<OsString> {
    filesystem
        .read_directory(path)
        .unwrap()
        .into_iter()
        .map(|entry| entry.name)
        .collect()
}

fn change_at<'a>(changes: &'a [ShareChange], path: &str) -> &'a ShareChange {
    changes
        .iter()
        .find(|change| change.path == Path::new(path))
        .unwrap()
}

fn copy_directory(source: &Path, destination: &Path) {
    fs::create_dir(destination).unwrap();
    for entry in fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let destination = destination.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_directory(&entry.path(), &destination);
        } else {
            fs::copy(entry.path(), destination).unwrap();
        }
    }
}

fn assert_not_found<T: std::fmt::Debug>(result: tascarrel_sharefs::ShareFsResult<T>) {
    let error = result.unwrap_err();
    assert!(matches!(error.error(), ShareFsError::NotFound { .. }));
}

/// Verifies promoted directories continue to merge live lower additions and
/// removals without recording unrelated host activity as pod changes.
#[test]
fn dynamically_merges_the_current_lower_directory() {
    let fixture = Fixture::new();
    fs::create_dir(fixture.lower.join("nested")).unwrap();
    fs::write(fixture.lower.join("nested/original"), b"original").unwrap();
    let filesystem = fixture.open();

    filesystem.create_file("nested/from-pod", 0o640).unwrap();
    fs::write(fixture.lower.join("nested/from-host"), b"host").unwrap();
    fs::remove_file(fixture.lower.join("nested/original")).unwrap();

    assert_eq!(
        entry_names(&filesystem, "nested"),
        [OsString::from("from-host"), OsString::from("from-pod")]
    );
    let changes = filesystem.changes().unwrap();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].path, Path::new("nested/from-pod"));
    assert!(changes[0].base.is_none());
}

/// Verifies canonical changes are globally sorted by raw path bytes rather
/// than by depth-first namespace traversal.
#[test]
fn changes_are_sorted_by_complete_raw_paths() {
    let fixture = Fixture::new();
    let filesystem = fixture.open();

    filesystem.create_directory("a", 0o755).unwrap();
    filesystem.create_file("a/x", 0o644).unwrap();
    filesystem.create_file("a-", 0o644).unwrap();

    assert_eq!(
        filesystem
            .changes()
            .unwrap()
            .into_iter()
            .map(|change| change.path)
            .collect::<Vec<_>>(),
        [
            PathBuf::from("a"),
            PathBuf::from("a-"),
            PathBuf::from("a/x"),
        ]
    );
}

/// Verifies partial copy-up captures one lower lease, shadows later host
/// changes, and persists both proposed data and the lease across reopening.
#[test]
fn copy_up_is_stable_across_lower_changes_and_reopening() {
    let fixture = Fixture::new();
    fs::write(fixture.lower.join("file"), b"abcdef").unwrap();
    let filesystem = fixture.open();

    filesystem.write_at("file", 2, b"XY").unwrap();
    let first_changes = filesystem.changes().unwrap();
    let first_change = change_at(&first_changes, "file");
    let first_lease = first_change.base.clone().unwrap();
    assert_eq!(filesystem.read_file("file").unwrap(), b"abXYef");
    assert_eq!(first_lease.version.kind, EntryKind::File);
    assert_ne!(
        first_lease.version.content_digest,
        first_change.proposed.as_ref().unwrap().content_digest
    );
    let objects = fs::read_dir(fixture.state.join("objects"))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(objects.len(), 1);
    assert_eq!(fs::read(objects[0].path()).unwrap(), b"abXYef");

    fs::write(fixture.lower.join("file"), b"host replacement").unwrap();
    assert_eq!(filesystem.read_file("file").unwrap(), b"abXYef");
    drop(filesystem);

    let reopened = fixture.open();
    assert_eq!(reopened.read_file("file").unwrap(), b"abXYef");
    let reopened_changes = reopened.changes().unwrap();
    assert_eq!(
        change_at(&reopened_changes, "file").base.as_ref(),
        Some(&first_lease)
    );
}

/// Verifies streaming descriptors resolve both live lower files and private
/// upper objects.
#[test]
fn opens_merged_regular_files_for_streaming() {
    let fixture = Fixture::new();
    fs::write(fixture.lower.join("file"), b"lower").unwrap();
    let filesystem = fixture.open();

    let mut lower = filesystem.open_file("file").unwrap();
    let mut lower_contents = Vec::new();
    lower.read_to_end(&mut lower_contents).unwrap();
    assert_eq!(lower_contents, b"lower");

    filesystem.write_file("file", b"upper").unwrap();
    let mut upper = filesystem.open_file("file").unwrap();
    let mut upper_contents = Vec::new();
    upper.read_to_end(&mut upper_contents).unwrap();
    assert_eq!(upper_contents, b"upper");
}

/// Verifies a complete replacement is serialized with revision validation.
#[test]
fn complete_write_requires_the_current_revision() {
    let fixture = Fixture::new();
    fs::write(fixture.lower.join("file"), b"before").unwrap();
    let filesystem = fixture.open();
    let before = ContentDigest::from_bytes(b"before");

    assert_eq!(
        filesystem
            .write_file_if_revision("file", before, b"after")
            .unwrap(),
        FileWriteOutcome::Written {
            revision: ContentDigest::from_bytes(b"after"),
        }
    );
    assert_eq!(filesystem.read_file("file").unwrap(), b"after");
    assert_eq!(
        filesystem
            .write_file_if_revision("file", before, b"stale")
            .unwrap(),
        FileWriteOutcome::Conflict {
            revision: ContentDigest::from_bytes(b"after"),
        }
    );
    assert_eq!(filesystem.read_file("file").unwrap(), b"after");
}

/// Verifies a synchronized copy of the complete upper state is independently
/// readable and retains the same canonical change set.
#[test]
fn synchronized_upper_snapshot_is_self_contained() {
    let fixture = Fixture::new();
    fs::write(fixture.lower.join("file"), b"base").unwrap();
    let filesystem = fixture.open();
    filesystem.write_file("file", b"proposal").unwrap();
    filesystem.create_file("new", 0o600).unwrap();
    filesystem.sync().unwrap();
    let expected = filesystem.changes().unwrap();

    let snapshot = fixture.temporary.path().join("snapshot");
    copy_directory(&fixture.state, &snapshot);
    let snapshotted = ShareFileSystem::open(&fixture.lower, &snapshot).unwrap();

    assert_eq!(snapshotted.read_file("file").unwrap(), b"proposal");
    assert_eq!(snapshotted.changes().unwrap(), expected);
}

/// Verifies an applied frozen revision can be cleared without losing the
/// dynamic view of the current lower directory.
#[test]
fn frozen_revision_clear_restores_the_live_lower() {
    let fixture = Fixture::new();
    fs::write(fixture.lower.join("document"), b"base").unwrap();
    let filesystem = Arc::new(fixture.open());
    filesystem
        .write_file("document", b"approved proposal")
        .unwrap();
    filesystem.create_file("pod-only", 0o600).unwrap();

    let frozen = filesystem.freeze().unwrap();
    assert_eq!(frozen.snapshot().unwrap().len(), 2);
    frozen.clear().unwrap();
    drop(frozen);

    assert_eq!(filesystem.read_file("document").unwrap(), b"base");
    assert_not_found(filesystem.metadata("pod-only"));
    fs::write(fixture.lower.join("host-later"), b"live").unwrap();
    assert_eq!(filesystem.read_file("host-later").unwrap(), b"live");
    assert!(filesystem.changes().unwrap().is_empty());
}

/// Verifies lease comparison uses content equality after a fast fingerprint
/// miss and treats directory membership changes conservatively.
#[test]
fn lower_lease_comparison_falls_back_to_content() {
    let fixture = Fixture::new();
    fs::write(fixture.lower.join("file"), b"base").unwrap();
    fs::set_permissions(
        fixture.lower.join("file"),
        fs::Permissions::from_mode(0o640),
    )
    .unwrap();
    fs::create_dir(fixture.lower.join("tree")).unwrap();
    let filesystem = fixture.open();
    filesystem.write_file("file", b"proposal").unwrap();
    filesystem.remove("tree").unwrap();
    let changes = filesystem.changes().unwrap();
    let file_lease = change_at(&changes, "file").base.as_ref().unwrap();
    let directory_lease = change_at(&changes, "tree").base.as_ref().unwrap();

    assert!(filesystem.lower_matches_lease("file", file_lease).unwrap());
    fs::remove_file(fixture.lower.join("file")).unwrap();
    fs::write(fixture.lower.join("file"), b"base").unwrap();
    fs::set_permissions(
        fixture.lower.join("file"),
        fs::Permissions::from_mode(0o640),
    )
    .unwrap();
    assert!(filesystem.lower_matches_lease("file", file_lease).unwrap());

    fs::write(fixture.lower.join("file"), b"changed").unwrap();
    assert!(!filesystem.lower_matches_lease("file", file_lease).unwrap());
    fs::write(fixture.lower.join("tree/host-child"), b"new").unwrap();
    assert!(
        !filesystem
            .lower_matches_lease("tree", directory_lease)
            .unwrap()
    );
}

/// Verifies an upper-created file shadows a later host collision and deleting
/// that transient upper entry reveals the live lower name without a net change.
#[test]
fn transient_upper_collision_can_reveal_the_lower_entry() {
    let fixture = Fixture::new();
    let filesystem = fixture.open();

    filesystem.create_file("collision", 0o600).unwrap();
    filesystem.write_file("collision", b"pod").unwrap();
    fs::write(fixture.lower.join("collision"), b"host").unwrap();
    assert_eq!(filesystem.read_file("collision").unwrap(), b"pod");

    filesystem.remove("collision").unwrap();
    assert_eq!(filesystem.read_file("collision").unwrap(), b"host");
    assert!(filesystem.changes().unwrap().is_empty());
}

/// Verifies a durable whiteout continues to hide a lower path after the host
/// removes and recreates it.
#[test]
fn whiteout_hides_recreated_lower_entry() {
    let fixture = Fixture::new();
    fs::write(fixture.lower.join("victim"), b"before").unwrap();
    let filesystem = fixture.open();

    filesystem.remove("victim").unwrap();
    fs::remove_file(fixture.lower.join("victim")).unwrap();
    fs::write(fixture.lower.join("victim"), b"after").unwrap();
    assert_not_found(filesystem.read_file("victim"));
    let changes = filesystem.changes().unwrap();
    let deletion = change_at(&changes, "victim");
    assert!(deletion.base.is_some());
    assert!(deletion.proposed.is_none());
    drop(filesystem);

    let reopened = fixture.open();
    assert_not_found(reopened.metadata("victim"));
}

/// Verifies deleting and recreating a lower directory makes the replacement
/// opaque so neither old nor subsequently added lower children reappear.
#[test]
fn recreated_lower_directory_is_opaque() {
    let fixture = Fixture::new();
    fs::create_dir(fixture.lower.join("tree")).unwrap();
    fs::write(fixture.lower.join("tree/old"), b"old").unwrap();
    let filesystem = fixture.open();

    filesystem.remove("tree/old").unwrap();
    filesystem.remove("tree").unwrap();
    filesystem.create_directory("tree", 0o755).unwrap();
    fs::write(fixture.lower.join("tree/later"), b"later").unwrap();

    assert!(entry_names(&filesystem, "tree").is_empty());
    let changes = filesystem.changes().unwrap();
    assert_eq!(changes.len(), 1);
    let replacement = change_at(&changes, "tree");
    assert!(replacement.opaque);
    assert_eq!(
        replacement.base.as_ref().unwrap().version.kind,
        EntryKind::Directory
    );
    assert_eq!(
        replacement.proposed.as_ref().unwrap().kind,
        EntryKind::Directory
    );
}

/// Verifies replacing a lower entry with another kind remains a change even
/// when the logical permission bits happen to match.
#[test]
fn entry_kind_replacement_is_never_a_semantic_noop() {
    let fixture = Fixture::new();
    fs::write(fixture.lower.join("entry"), b"").unwrap();
    fs::set_permissions(
        fixture.lower.join("entry"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let filesystem = fixture.open();

    filesystem.remove("entry").unwrap();
    filesystem.create_directory("entry", 0o755).unwrap();

    let changes = filesystem.changes().unwrap();
    let replacement = change_at(&changes, "entry");
    assert_eq!(
        replacement.base.as_ref().unwrap().version.kind,
        EntryKind::File
    );
    assert_eq!(
        replacement.proposed.as_ref().unwrap().kind,
        EntryKind::Directory
    );
}

/// Verifies renaming a lower file over another lower file atomically retains
/// source and destination leases and survives reopening.
#[test]
fn lower_file_rename_records_both_path_changes() {
    let fixture = Fixture::new();
    fs::write(fixture.lower.join("source"), b"source contents").unwrap();
    fs::write(fixture.lower.join("destination"), b"destination contents").unwrap();
    let filesystem = fixture.open();

    filesystem.rename("source", "destination").unwrap();
    assert_not_found(filesystem.metadata("source"));
    assert_eq!(
        filesystem.read_file("destination").unwrap(),
        b"source contents"
    );
    let changes = filesystem.changes().unwrap();
    assert_eq!(changes.len(), 2);
    let source = change_at(&changes, "source");
    let destination = change_at(&changes, "destination");
    assert!(source.base.is_some());
    assert!(source.proposed.is_none());
    assert!(destination.base.is_some());
    assert_eq!(
        destination.proposed.as_ref().unwrap().content_digest,
        source.base.as_ref().unwrap().version.content_digest
    );
    drop(filesystem);

    let reopened = fixture.open();
    assert_not_found(reopened.metadata("source"));
    assert_eq!(
        reopened.read_file("destination").unwrap(),
        b"source contents"
    );
}

/// Verifies a failed rename does not copy up its source or make later lower
/// changes invisible.
#[test]
fn failed_rename_leaves_the_source_live() {
    let fixture = Fixture::new();
    fs::write(fixture.lower.join("source"), b"before").unwrap();
    let filesystem = fixture.open();

    let error = filesystem
        .rename("source", "missing/destination")
        .unwrap_err();
    assert!(matches!(error.error(), ShareFsError::NotFound { .. }));
    fs::write(fixture.lower.join("source"), b"after").unwrap();

    assert_eq!(filesystem.read_file("source").unwrap(), b"after");
    assert!(filesystem.changes().unwrap().is_empty());
}

/// Verifies renaming a path to itself still checks that the source exists.
#[test]
fn same_path_rename_requires_an_existing_source() {
    let fixture = Fixture::new();
    let filesystem = fixture.open();

    assert_not_found(filesystem.rename("missing", "missing"));
}

/// Verifies writing the exact captured contents and mode does not produce a
/// semantic change even though the file has been copied into the upper.
#[test]
fn semantic_noop_is_omitted_from_changes() {
    let fixture = Fixture::new();
    fs::write(fixture.lower.join("same"), b"same").unwrap();
    fs::set_permissions(
        fixture.lower.join("same"),
        fs::Permissions::from_mode(0o640),
    )
    .unwrap();
    let filesystem = fixture.open();

    filesystem.write_file("same", b"same").unwrap();
    filesystem.set_mode("same", 0o640).unwrap();

    assert!(filesystem.changes().unwrap().is_empty());
}

/// Verifies symbolic links and non-UTF-8 Unix names remain byte-preserving in
/// the merged namespace and durable upper state.
#[test]
fn preserves_symlinks_and_non_utf8_names() {
    let fixture = Fixture::new();
    symlink(Path::new("../target"), fixture.lower.join("lower-link")).unwrap();
    let raw_name = OsString::from_vec(vec![b'n', b'a', b'm', b'e', 0xff]);
    let raw_path = PathBuf::from(&raw_name);
    let filesystem = fixture.open();

    assert_eq!(
        filesystem.read_link("lower-link").unwrap(),
        Path::new("../target")
    );
    filesystem
        .create_symlink(&raw_path, Path::new("target"))
        .unwrap();
    assert_eq!(
        filesystem.read_link(&raw_path).unwrap(),
        Path::new("target")
    );
    assert!(entry_names(&filesystem, "").contains(&raw_name));
    drop(filesystem);

    let reopened = fixture.open();
    assert_eq!(reopened.read_link(&raw_path).unwrap(), Path::new("target"));
}

/// Verifies interrupted staging data and unreferenced objects are collected
/// when a durable upper state is opened.
#[test]
fn opening_collects_incomplete_and_orphaned_objects() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.state.join("staging")).unwrap();
    fs::create_dir_all(fixture.state.join("objects")).unwrap();
    fs::write(fixture.state.join("staging/incomplete"), b"partial").unwrap();
    fs::write(fixture.state.join("objects/orphan"), b"orphan").unwrap();

    let filesystem = fixture.open();

    assert!(entry_names(&filesystem, "").is_empty());
    assert!(
        fs::read_dir(fixture.state.join("staging"))
            .unwrap()
            .next()
            .is_none()
    );
    assert!(
        fs::read_dir(fixture.state.join("objects"))
            .unwrap()
            .next()
            .is_none()
    );
}

/// Verifies opening rejects an upper state whose database references a missing
/// regular-file object.
#[test]
fn opening_rejects_a_missing_referenced_object() {
    let fixture = Fixture::new();
    let filesystem = fixture.open();
    filesystem.create_file("file", 0o600).unwrap();
    drop(filesystem);
    let object = fs::read_dir(fixture.state.join("objects"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap();
    fs::remove_file(object.path()).unwrap();

    let error = ShareFileSystem::open(&fixture.lower, &fixture.state).unwrap_err();
    assert!(matches!(error.error(), ShareFsError::CorruptState));
}

/// Verifies only one filesystem instance can mutate a durable upper state at a
/// time and that dropping the owner releases the state.
#[test]
fn durable_state_has_one_exclusive_owner() {
    let fixture = Fixture::new();
    let first = fixture.open();

    let error = ShareFileSystem::open(&fixture.lower, &fixture.state).unwrap_err();
    assert!(matches!(error.error(), ShareFsError::StateInUse));
    drop(first);

    fixture.open();
}

/// Verifies unsafe absolute and parent-relative paths never reach the lower
/// filesystem.
#[test]
fn rejects_paths_outside_the_share_namespace() {
    let fixture = Fixture::new();
    let filesystem = fixture.open();

    let absolute = filesystem.metadata("/outside").unwrap_err();
    assert!(matches!(absolute.error(), ShareFsError::InvalidPath { .. }));
    let parent = filesystem.create_file("../outside", 0o600).unwrap_err();
    assert!(matches!(parent.error(), ShareFsError::InvalidPath { .. }));
    let target_with_nul = PathBuf::from(OsString::from_vec(b"target\0suffix".to_vec()));
    let target = filesystem
        .create_symlink("link", target_with_nul)
        .unwrap_err();
    assert!(matches!(target.error(), ShareFsError::InvalidPath { .. }));
}
