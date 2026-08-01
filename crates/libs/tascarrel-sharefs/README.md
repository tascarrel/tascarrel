# Tascarrel ShareFS

`tascarrel-sharefs` implements the durable copy-on-write mechanics for an
approval-gated host share. It merges a live lower directory with one private
upper state. Reads of untouched paths use the current lower filesystem. Pod
mutations create upper nodes or whiteouts without writing to the lower
directory.

The crate provides both the path-based storage API and a FUSE adapter.
The guest daemon (`guestd`) owns each kernel mount, assigns the pod image user's
mapped UID and GID, snapshots the upper at a frozen revision boundary, and asks
hostd to apply an explicitly approved revision.

Merged regular files can be read into memory with `ShareFileSystem::read_file`
or opened as pinned descriptors with `ShareFileSystem::open_file`. Guestd uses
the descriptor interface when streaming previews and downloads so file size
does not determine its memory use.

## Merge Model

An absent upper entry delegates to the lower directory. A present upper entry
shadows an equal lower name. A whiteout hides the lower name. Directories merge
their upper and current lower children unless the directory is opaque.

Deleting and recreating a lower-backed directory makes the replacement opaque.
This prevents deleted lower children from reappearing. A newly created
directory remains dynamically merged, so a later host-created child appears
unless an upper entry or whiteout shadows it.

Regular-file copy-up stores only the proposed upper file. It hashes the lower
contents while capturing the lease, but does not retain a duplicate base blob.
`ShareFileSystem::changes` returns the captured lower lease and proposed
version. The lease includes inode identity, size, mode, mtime, and ctime for a
cheap comparison. `ShareFileSystem::lower_matches_lease` performs that
comparison and falls back to hashing regular files or symbolic-link targets
when the fingerprint differs.

## Durable State

Each private upper uses one state directory:

```text
state/
├── state.lock
├── index.sqlite3
├── index.sqlite3-shm
├── index.sqlite3-wal
├── objects/
└── staging/
```

The SQLite index maps raw Unix names to node identifiers and records whiteouts,
opaque directories, logical metadata, and lower leases. Regular-file contents
live in `objects/` under generated identifiers. New objects are synchronized in
`staging/`, renamed into `objects/`, and then referenced by a database
transaction. Opening a state removes interrupted staging files, unreachable
nodes, and orphaned objects. An exclusive lock prevents two processes from
mutating the same upper concurrently.

`ShareFileSystem::sync` flushes referenced objects and checkpoints the database.
This is the boundary to use before taking a filesystem snapshot of the complete
state directory. `ShareFileSystem::freeze` first drains ordinary operations and
blocks new ones. Its frozen handle can synchronize and enumerate an exact
revision, then clear that upper only after hostd acknowledges a successful
apply.

## Example

```rust
use std::path::Path;

use tascarrel_sharefs::ShareFileSystem;

fn propose_note(lower: &Path, state: &Path) -> tascarrel_sharefs::ShareFsResult<()> {
    let filesystem = ShareFileSystem::open(lower, state)?;
    filesystem.create_file("notes.txt", 0o600)?;
    filesystem.write_file("notes.txt", b"pod proposal\n")?;

    let changes = filesystem.changes()?;
    assert_eq!(changes.len(), 1);
    assert!(changes[0].base.is_none());
    Ok(())
}
```

## Integration Boundary

`MountedShareFileSystem` translates inode- and handle-based FUSE requests into
the path API and owns the background kernel session. The crate deliberately
does not mutate a host share during approval: guestd snapshots and transfers an
exact upper revision, while hostd validates the captured leases, presents
conflicts, and applies approved changes to its pinned host directory.

Kernel mounts require `/dev/fuse` and a mounted FUSE control filesystem at
`/sys/fs/fuse/connections`. Shutdown aborts the specific connection through
that control filesystem before unmounting and joining its worker, which gives
guestd a bounded teardown path even when the kernel is waiting on a FUSE
request.
