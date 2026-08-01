//! Live-lower namespace resolution and private upper mutations.
//!
//! [`ShareFileSystem`] is the public, serialized path-based interface. This
//! module retains its durable namespace mechanics separately from that public
//! boundary so the FUSE adapter can reuse the same state transitions.

mod interface;
mod support;

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::Read as _;
use std::io::Write as _;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::FileExt as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

pub use interface::FrozenShareFileSystem;
pub use interface::ShareFileSystem;
use reportify::Report;
use sha2::Digest as _;
use sha2::Sha256;
use support::acquire_state_lock;
use support::base_record;
use support::clean_staging_directory;
use support::components_to_path;
use support::concurrent_change;
use support::digest_bytes;
use support::digest_file;
use support::ensure_private_directory;
use support::entry_kind;
use support::entry_type_error;
use support::entry_type_error_for_kind;
use support::io_error;
use support::lower_metadata;
use support::matches_lease_fingerprint;
use support::metadata_value;
use support::normalize_non_root_path;
use support::normalize_path;
use support::now;
use support::open_read_only_file;
use support::optional_symlink_metadata;
use support::prepare_lower_directory;
use support::prepare_state_directory;
use support::read_lower_directory;
use support::real_lower_directory;
use support::remove_file_if_exists;
use support::same_fingerprint;
use support::split_parent;
use support::symlink_metadata;
use support::sync_directory;
use uuid::Uuid;

use crate::ContentDigest;
use crate::DirectoryEntry;
use crate::EntryKind;
use crate::EntryMetadata;
use crate::EntryVersion;
use crate::LowerLease;
use crate::ShareChange;
use crate::ShareFsError;
use crate::ShareFsResult;
use crate::state::BaseRecord;
use crate::state::EntryState;
use crate::state::NewNode;
use crate::state::NodeRecord;
use crate::state::ROOT_NODE_ID;
use crate::state::State;

const DATABASE_FILE: &str = "index.sqlite3";
const LOCK_FILE: &str = "state.lock";
const OBJECTS_DIRECTORY: &str = "objects";
const STAGING_DIRECTORY: &str = "staging";
const LOGICAL_MODE_MASK: u32 = 0o7777;

struct Core {
    lower: PathBuf,
    state_root: PathBuf,
    objects: PathBuf,
    staging: PathBuf,
    database: PathBuf,
    _state_lock: File,
    namespace: State,
}

impl Core {
    fn metadata(&self, path: &Path) -> ShareFsResult<EntryMetadata> {
        match self.resolve(path)? {
            ResolvedEntry::Upper(node) => self.upper_metadata(path, &node),
            ResolvedEntry::Lower(lower) => lower_metadata(&lower, path),
        }
    }

    fn read_directory(&self, path: &Path) -> ShareFsResult<Vec<DirectoryEntry>> {
        match self.resolve(path)? {
            ResolvedEntry::Lower(lower) => read_lower_directory(&lower, path),
            ResolvedEntry::Upper(node) => {
                if node.kind != EntryKind::Directory {
                    return Err(Report::new(ShareFsError::NotDirectory {
                        path: path.to_owned(),
                    }));
                }
                let mut entries = BTreeMap::new();
                if node.merge_lower
                    && !node.opaque
                    && let Some(lower) = real_lower_directory(&self.lower.join(path))?
                {
                    for entry in read_lower_directory(&lower, path)? {
                        entries.insert(entry.name.clone(), entry);
                    }
                }
                for override_entry in self.namespace.entries(node.id)? {
                    match override_entry.state {
                        EntryState::Whiteout => {
                            entries.remove(&override_entry.name);
                        }
                        EntryState::Present(child_id) => {
                            let child = self.namespace.node(child_id)?;
                            let child_path = path.join(&override_entry.name);
                            let metadata = self.upper_metadata(&child_path, &child)?;
                            entries.insert(
                                override_entry.name.clone(),
                                DirectoryEntry {
                                    name: override_entry.name,
                                    metadata,
                                },
                            );
                        }
                    }
                }
                Ok(entries.into_values().collect())
            }
        }
    }

    fn open_file(&self, path: &Path) -> ShareFsResult<File> {
        let file = match self.resolve(path)? {
            ResolvedEntry::Lower(lower) => {
                let metadata = symlink_metadata(&lower, "inspect a lower file")?;
                if entry_kind(&metadata, path)? != EntryKind::File {
                    return Err(entry_type_error(path, &metadata));
                }
                open_read_only_file(&lower)
            }
            ResolvedEntry::Upper(node) => {
                if node.kind != EntryKind::File {
                    return Err(entry_type_error_for_kind(path, node.kind));
                }
                open_read_only_file(&self.object_path(&node)?)
            }
        }?;
        let metadata = file
            .metadata()
            .map_err(|source| io_error("inspect an opened share file", source))?;
        if entry_kind(&metadata, path)? != EntryKind::File {
            return Err(entry_type_error(path, &metadata));
        }
        Ok(file)
    }

    fn read_file(&self, path: &Path) -> ShareFsResult<Vec<u8>> {
        let mut file = self.open_file(path)?;
        let mut contents = Vec::new();
        file.read_to_end(&mut contents)
            .map_err(|source| io_error("read a share file", source))?;
        Ok(contents)
    }

    fn write_file(&mut self, path: &Path, contents: &[u8]) -> ShareFsResult<()> {
        let node = self.materialize_regular_file(path, false)?;
        self.namespace.mark_content_changed(node.id, now())?;
        let object = self.object_path(&node)?;
        let mut file = OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&object)
            .map_err(|source| io_error("open an upper file for replacement", source))?;
        file.write_all(contents)
            .map_err(|source| io_error("replace upper file contents", source))
    }

    fn write_at(&mut self, path: &Path, offset: u64, contents: &[u8]) -> ShareFsResult<()> {
        let node = self.materialize_regular_file(path, true)?;
        self.namespace.mark_content_changed(node.id, now())?;
        let object = self.object_path(&node)?;
        let file = OpenOptions::new()
            .write(true)
            .open(&object)
            .map_err(|source| io_error("open an upper file for writing", source))?;
        file.write_all_at(contents, offset)
            .map_err(|source| io_error("write upper file contents", source))
    }

    fn set_file_length(&mut self, path: &Path, length: u64) -> ShareFsResult<()> {
        let node = self.materialize_regular_file(path, true)?;
        self.namespace.mark_content_changed(node.id, now())?;
        let object = self.object_path(&node)?;
        let file = OpenOptions::new()
            .write(true)
            .open(&object)
            .map_err(|source| io_error("open an upper file for truncation", source))?;
        file.set_len(length)
            .map_err(|source| io_error("truncate an upper file", source))
    }

    fn create_file(&mut self, path: &Path, mode: u32) -> ShareFsResult<()> {
        self.ensure_absent(path)?;
        let (parent, name) = split_parent(path)?;
        let parent_id = self.ensure_upper_directory(parent)?;
        let existing = self.namespace.entry(parent_id, name)?;
        let base = existing.as_ref().and_then(|entry| entry.base.as_ref());
        let object_name = self.create_empty_object()?;
        let node = NewNode {
            kind: EntryKind::File,
            object_name: Some(object_name.clone()),
            symlink_target: None,
            mode,
            modified_at: now(),
            merge_lower: false,
            opaque: false,
            metadata_changed: true,
        };
        if let Err(error) = self.namespace.install_node(parent_id, name, &node, base) {
            remove_file_if_exists(&self.objects.join(object_name))?;
            return Err(error);
        }
        self.collect_garbage()
    }

    fn create_directory(&mut self, path: &Path, mode: u32) -> ShareFsResult<()> {
        self.ensure_absent(path)?;
        let (parent, name) = split_parent(path)?;
        let parent_id = self.ensure_upper_directory(parent)?;
        let existing = self.namespace.entry(parent_id, name)?;
        let base = existing.as_ref().and_then(|entry| entry.base.as_ref());
        let opaque = base.is_some_and(|base| base.version.kind == EntryKind::Directory);
        self.namespace.install_node(
            parent_id,
            name,
            &NewNode {
                kind: EntryKind::Directory,
                object_name: None,
                symlink_target: None,
                mode,
                modified_at: now(),
                merge_lower: true,
                opaque,
                metadata_changed: true,
            },
            base,
        )?;
        self.collect_garbage()
    }

    fn create_symlink(&mut self, path: &Path, target: &Path) -> ShareFsResult<()> {
        self.ensure_absent(path)?;
        let (parent, name) = split_parent(path)?;
        let parent_id = self.ensure_upper_directory(parent)?;
        let existing = self.namespace.entry(parent_id, name)?;
        let base = existing.as_ref().and_then(|entry| entry.base.as_ref());
        self.namespace.install_node(
            parent_id,
            name,
            &NewNode {
                kind: EntryKind::Symlink,
                object_name: None,
                symlink_target: Some(target.as_os_str().to_owned()),
                mode: 0o777,
                modified_at: now(),
                merge_lower: false,
                opaque: false,
                metadata_changed: true,
            },
            base,
        )?;
        self.collect_garbage()
    }

    fn read_link(&self, path: &Path) -> ShareFsResult<PathBuf> {
        match self.resolve(path)? {
            ResolvedEntry::Lower(lower) => {
                let metadata = symlink_metadata(&lower, "inspect a lower symbolic link")?;
                if entry_kind(&metadata, path)? != EntryKind::Symlink {
                    return Err(entry_type_error(path, &metadata));
                }
                fs::read_link(lower)
                    .map_err(|source| io_error("read a lower symbolic link", source))
            }
            ResolvedEntry::Upper(node) => {
                if node.kind != EntryKind::Symlink {
                    return Err(entry_type_error_for_kind(path, node.kind));
                }
                node.symlink_target
                    .map(PathBuf::from)
                    .ok_or_else(|| Report::new(ShareFsError::CorruptState))
            }
        }
    }

    fn remove(&mut self, path: &Path) -> ShareFsResult<()> {
        let resolved = self.resolve(path)?;
        let kind = match &resolved {
            ResolvedEntry::Upper(node) => node.kind,
            ResolvedEntry::Lower(lower) => {
                let metadata = symlink_metadata(lower, "inspect a removed lower entry")?;
                entry_kind(&metadata, path)?
            }
        };
        if kind == EntryKind::Directory && !self.read_directory(path)?.is_empty() {
            return Err(Report::new(ShareFsError::DirectoryNotEmpty {
                path: path.to_owned(),
            }));
        }
        let (parent, name) = split_parent(path)?;
        let parent_id = self.ensure_upper_directory(parent)?;
        let current_override = self.namespace.entry(parent_id, name)?;
        let captured_base;
        let base = if let Some(entry) = current_override.as_ref() {
            entry.base.as_ref()
        } else {
            captured_base = self.capture_lower_entry(path, None)?;
            Some(&captured_base)
        };
        self.namespace.remove_entry(parent_id, name, base)?;
        self.collect_garbage()
    }

    fn rename(&mut self, source: &Path, destination: &Path) -> ShareFsResult<()> {
        let resolved_source = self.resolve(source)?;
        let source_kind = resolved_entry_kind(&resolved_source, source)?;
        if source == destination {
            return Ok(());
        }
        if destination.starts_with(source) {
            return Err(Report::new(ShareFsError::InvalidPath {
                path: destination.to_owned(),
            }));
        }
        self.validate_rename_source(source, &resolved_source, source_kind)?;
        self.validate_rename_destination(destination, source_kind)?;
        let (destination_parent, destination_name) = split_parent(destination)?;
        self.require_directory(destination_parent)?;
        self.materialize_rename_source(source)?;
        let (source_parent, source_name) = split_parent(source)?;
        let source_parent_id = self.ensure_upper_directory(source_parent)?;
        let destination_parent_id = self.ensure_upper_directory(destination_parent)?;
        let existing_destination = self
            .namespace
            .entry(destination_parent_id, destination_name)?;
        let captured_destination;
        let destination_base = if let Some(entry) = existing_destination.as_ref() {
            entry.base.as_ref()
        } else {
            captured_destination = match optional_symlink_metadata(
                &self.lower.join(destination),
                "inspect a lower rename destination",
            )? {
                Some(_) => Some(self.capture_lower_entry(destination, None)?),
                None => None,
            };
            captured_destination.as_ref()
        };
        self.namespace.rename_entry(
            source_parent_id,
            source_name,
            destination_parent_id,
            destination_name,
            destination_base,
        )?;
        self.collect_garbage()
    }

    fn validate_rename_source(
        &mut self,
        source: &Path,
        resolved: &ResolvedEntry,
        kind: EntryKind,
    ) -> ShareFsResult<()> {
        if kind != EntryKind::Directory {
            return Ok(());
        }
        let ResolvedEntry::Upper(node) = resolved else {
            return Err(Report::new(ShareFsError::LowerDirectoryRename {
                path: source.to_owned(),
            }));
        };
        let (parent, name) = split_parent(source)?;
        let parent_id = self.ensure_upper_directory(parent)?;
        let entry = self
            .namespace
            .entry(parent_id, name)?
            .ok_or_else(|| Report::new(ShareFsError::CorruptState))?;
        if node.merge_lower && !node.opaque && entry.base.is_some() {
            return Err(Report::new(ShareFsError::LowerDirectoryRename {
                path: source.to_owned(),
            }));
        }
        Ok(())
    }

    fn validate_rename_destination(
        &self,
        destination_path: &Path,
        source_kind: EntryKind,
    ) -> ShareFsResult<()> {
        let destination = match self.resolve(destination_path) {
            Ok(destination) => destination,
            Err(error) if matches!(error.error(), ShareFsError::NotFound { .. }) => return Ok(()),
            Err(error) => return Err(error),
        };
        let destination_kind = resolved_entry_kind(&destination, destination_path)?;
        match (source_kind, destination_kind) {
            (EntryKind::Directory, EntryKind::Directory) => {
                if !self.read_directory(destination_path)?.is_empty() {
                    return Err(Report::new(ShareFsError::DirectoryNotEmpty {
                        path: destination_path.to_owned(),
                    }));
                }
            }
            (EntryKind::Directory, _) => {
                return Err(Report::new(ShareFsError::NotDirectory {
                    path: destination_path.to_owned(),
                }));
            }
            (_, EntryKind::Directory) => {
                return Err(Report::new(ShareFsError::IsDirectory {
                    path: destination_path.to_owned(),
                }));
            }
            _ => {}
        }
        Ok(())
    }

    fn set_mode(&mut self, path: &Path, mode: u32) -> ShareFsResult<()> {
        let node = match self.resolve(path)? {
            ResolvedEntry::Upper(node) => node,
            ResolvedEntry::Lower(lower) => {
                let metadata = symlink_metadata(&lower, "inspect lower entry metadata")?;
                match entry_kind(&metadata, path)? {
                    EntryKind::File => self.materialize_regular_file(path, true)?,
                    EntryKind::Directory => {
                        let id = self.ensure_upper_directory(path)?;
                        self.namespace.node(id)?
                    }
                    EntryKind::Symlink => {
                        return Err(Report::new(ShareFsError::UnsupportedEntryType {
                            path: path.to_owned(),
                        }));
                    }
                }
            }
        };
        if node.kind == EntryKind::Symlink {
            return Err(Report::new(ShareFsError::UnsupportedEntryType {
                path: path.to_owned(),
            }));
        }
        self.namespace.set_mode(node.id, mode, now())
    }

    fn changes(&self) -> ShareFsResult<Vec<ShareChange>> {
        let mut changes = Vec::new();
        self.collect_changes(ROOT_NODE_ID, Path::new(""), &mut changes)?;
        changes.sort_by(|left, right| {
            left.path
                .as_os_str()
                .as_bytes()
                .cmp(right.path.as_os_str().as_bytes())
        });
        Ok(changes)
    }

    fn lower_matches_lease(&self, path: &Path, lease: &LowerLease) -> ShareFsResult<bool> {
        let lower = self.lower.join(path);
        let Some(metadata) = optional_symlink_metadata(&lower, "inspect a leased lower entry")?
        else {
            return Ok(false);
        };
        let kind = entry_kind(&metadata, path)?;
        if matches_lease_fingerprint(&metadata, kind, lease) {
            return Ok(true);
        }
        if kind == EntryKind::Directory || lease.version.kind == EntryKind::Directory {
            return Ok(false);
        }
        let current = self.capture_lower_entry(path, None)?;
        Ok(current.version == lease.version)
    }

    fn sync(&mut self) -> ShareFsResult<()> {
        for object in self.namespace.referenced_objects()? {
            File::open(self.objects.join(object))
                .map_err(|source| io_error("open an upper object for synchronization", source))?
                .sync_all()
                .map_err(|source| io_error("synchronize an upper object", source))?;
        }
        self.namespace.checkpoint()?;
        File::open(&self.database)
            .map_err(|source| io_error("open the share namespace database", source))?
            .sync_all()
            .map_err(|source| io_error("synchronize the share namespace database", source))?;
        sync_directory(&self.objects)?;
        sync_directory(&self.state_root)
    }

    fn resolve(&self, path: &Path) -> ShareFsResult<ResolvedEntry> {
        if path.as_os_str().is_empty() {
            return Ok(ResolvedEntry::Upper(self.namespace.node(ROOT_NODE_ID)?));
        }
        let mut upper_directory = Some(ROOT_NODE_ID);
        let mut lower_path = Some(self.lower.clone());
        let components = path.components().collect::<Vec<_>>();
        for (index, component) in components.iter().enumerate() {
            let Component::Normal(name) = component else {
                return Err(Report::new(ShareFsError::InvalidPath {
                    path: path.to_owned(),
                }));
            };
            let final_component = index + 1 == components.len();
            if let Some(parent) = upper_directory
                && let Some(entry) = self.namespace.entry(parent, name)?
            {
                match entry.state {
                    EntryState::Whiteout => {
                        return Err(Report::new(ShareFsError::NotFound {
                            path: path.to_owned(),
                        }));
                    }
                    EntryState::Present(node_id) => {
                        let node = self.namespace.node(node_id)?;
                        if final_component {
                            return Ok(ResolvedEntry::Upper(node));
                        }
                        if node.kind != EntryKind::Directory {
                            return Err(Report::new(ShareFsError::NotDirectory {
                                path: components_to_path(&components[..=index]),
                            }));
                        }
                        upper_directory = Some(node.id);
                        lower_path = if node.merge_lower && !node.opaque {
                            lower_path.map(|parent| parent.join(name))
                        } else {
                            None
                        };
                        continue;
                    }
                }
            }
            let Some(candidate) = lower_path.take().map(|parent| parent.join(name)) else {
                return Err(Report::new(ShareFsError::NotFound {
                    path: path.to_owned(),
                }));
            };
            let metadata = optional_symlink_metadata(&candidate, "inspect a lower share entry")?
                .ok_or_else(|| {
                    Report::new(ShareFsError::NotFound {
                        path: path.to_owned(),
                    })
                })?;
            if final_component {
                entry_kind(&metadata, path)?;
                return Ok(ResolvedEntry::Lower(candidate));
            }
            if entry_kind(&metadata, path)? != EntryKind::Directory {
                return Err(Report::new(ShareFsError::NotDirectory {
                    path: components_to_path(&components[..=index]),
                }));
            }
            upper_directory = None;
            lower_path = Some(candidate);
        }
        Err(Report::new(ShareFsError::NotFound {
            path: path.to_owned(),
        }))
    }

    fn ensure_upper_directory(&mut self, path: &Path) -> ShareFsResult<i64> {
        if path.as_os_str().is_empty() {
            return Ok(ROOT_NODE_ID);
        }
        let (parent, name) = split_parent(path)?;
        let parent_id = self.ensure_upper_directory(parent)?;
        if let Some(entry) = self.namespace.entry(parent_id, name)? {
            return match entry.state {
                EntryState::Whiteout => Err(Report::new(ShareFsError::NotFound {
                    path: path.to_owned(),
                })),
                EntryState::Present(node_id) => {
                    let node = self.namespace.node(node_id)?;
                    if node.kind == EntryKind::Directory {
                        Ok(node.id)
                    } else {
                        Err(Report::new(ShareFsError::NotDirectory {
                            path: path.to_owned(),
                        }))
                    }
                }
            };
        }
        let base = self.capture_lower_entry(path, None)?;
        if base.version.kind != EntryKind::Directory {
            return Err(Report::new(ShareFsError::NotDirectory {
                path: path.to_owned(),
            }));
        }
        let node = self.namespace.install_node(
            parent_id,
            name,
            &NewNode {
                kind: EntryKind::Directory,
                object_name: None,
                symlink_target: None,
                mode: base.version.mode,
                modified_at: base.modified_at,
                merge_lower: true,
                opaque: false,
                metadata_changed: false,
            },
            Some(&base),
        )?;
        Ok(node.id)
    }

    fn materialize_regular_file(
        &mut self,
        path: &Path,
        preserve_contents: bool,
    ) -> ShareFsResult<NodeRecord> {
        match self.resolve(path)? {
            ResolvedEntry::Upper(node) => {
                if node.kind != EntryKind::File {
                    return Err(entry_type_error_for_kind(path, node.kind));
                }
                Ok(node)
            }
            ResolvedEntry::Lower(lower) => {
                let metadata = symlink_metadata(&lower, "inspect a copied-up lower file")?;
                if entry_kind(&metadata, path)? != EntryKind::File {
                    return Err(entry_type_error(path, &metadata));
                }
                let (object_name, staging_path, mut staging_file) = self.start_object()?;
                let base = if preserve_contents {
                    match self.capture_lower_entry(path, Some(&mut staging_file)) {
                        Ok(base) => base,
                        Err(error) => {
                            drop(staging_file);
                            remove_file_if_exists(&staging_path)?;
                            return Err(error);
                        }
                    }
                } else {
                    match self.capture_lower_entry(path, None) {
                        Ok(base) => base,
                        Err(error) => {
                            drop(staging_file);
                            remove_file_if_exists(&staging_path)?;
                            return Err(error);
                        }
                    }
                };
                self.commit_object(&staging_path, &object_name, staging_file)?;
                let (parent, name) = split_parent(path)?;
                let parent_id = self.ensure_upper_directory(parent)?;
                let node = NewNode {
                    kind: EntryKind::File,
                    object_name: Some(object_name.clone()),
                    symlink_target: None,
                    mode: base.version.mode,
                    modified_at: base.modified_at,
                    merge_lower: false,
                    opaque: false,
                    metadata_changed: false,
                };
                match self
                    .namespace
                    .install_node(parent_id, name, &node, Some(&base))
                {
                    Ok(node) => Ok(node),
                    Err(error) => {
                        remove_file_if_exists(&self.objects.join(object_name))?;
                        Err(error)
                    }
                }
            }
        }
    }

    fn materialize_rename_source(&mut self, path: &Path) -> ShareFsResult<NodeRecord> {
        match self.resolve(path)? {
            ResolvedEntry::Upper(node) => Ok(node),
            ResolvedEntry::Lower(lower) => {
                let metadata = symlink_metadata(&lower, "inspect a rename source")?;
                match entry_kind(&metadata, path)? {
                    EntryKind::File => self.materialize_regular_file(path, true),
                    EntryKind::Symlink => self.materialize_lower_symlink(path),
                    EntryKind::Directory => Err(Report::new(ShareFsError::LowerDirectoryRename {
                        path: path.to_owned(),
                    })),
                }
            }
        }
    }

    fn materialize_lower_symlink(&mut self, path: &Path) -> ShareFsResult<NodeRecord> {
        let (base, target) = self.capture_lower_symlink(path)?;
        let (parent, name) = split_parent(path)?;
        let parent_id = self.ensure_upper_directory(parent)?;
        self.namespace.install_node(
            parent_id,
            name,
            &NewNode {
                kind: EntryKind::Symlink,
                object_name: None,
                symlink_target: Some(target),
                mode: base.version.mode,
                modified_at: base.modified_at,
                merge_lower: false,
                opaque: false,
                metadata_changed: false,
            },
            Some(&base),
        )
    }

    fn capture_lower_entry(
        &self,
        path: &Path,
        mut copy_to: Option<&mut File>,
    ) -> ShareFsResult<BaseRecord> {
        let lower = self.lower.join(path);
        let before = symlink_metadata(&lower, "inspect a lower entry before capture")?;
        let kind = entry_kind(&before, path)?;
        let digest = match kind {
            EntryKind::File => {
                let mut source = open_read_only_file(&lower)?;
                let opened_before = source
                    .metadata()
                    .map_err(|source| io_error("inspect an opened lower file", source))?;
                if !same_fingerprint(&before, &opened_before) {
                    return Err(concurrent_change(path));
                }
                let mut hasher = Sha256::new();
                let mut buffer = vec![0_u8; 128 * 1024];
                loop {
                    let count = source
                        .read(&mut buffer)
                        .map_err(|source| io_error("read a lower file during capture", source))?;
                    if count == 0 {
                        break;
                    }
                    hasher.update(&buffer[..count]);
                    if let Some(destination) = copy_to.as_deref_mut() {
                        destination.write_all(&buffer[..count]).map_err(|source| {
                            io_error("copy a lower file into the upper", source)
                        })?;
                    }
                }
                let opened_after = source
                    .metadata()
                    .map_err(|source| io_error("reinspect an opened lower file", source))?;
                let path_after = symlink_metadata(&lower, "reinspect a lower file after capture")?;
                if !same_fingerprint(&before, &opened_after)
                    || !same_fingerprint(&before, &path_after)
                {
                    return Err(concurrent_change(path));
                }
                Some(ContentDigest(hasher.finalize().into()))
            }
            EntryKind::Symlink => {
                return self.capture_lower_symlink(path).map(|(base, _target)| base);
            }
            EntryKind::Directory => {
                let after = symlink_metadata(&lower, "reinspect a lower directory after capture")?;
                if !same_fingerprint(&before, &after) {
                    return Err(concurrent_change(path));
                }
                None
            }
        };
        Ok(base_record(&before, kind, digest))
    }

    fn capture_lower_symlink(&self, path: &Path) -> ShareFsResult<(BaseRecord, OsString)> {
        let lower = self.lower.join(path);
        let before = symlink_metadata(&lower, "inspect a lower symbolic link before capture")?;
        if entry_kind(&before, path)? != EntryKind::Symlink {
            return Err(entry_type_error(path, &before));
        }
        let target = fs::read_link(&lower)
            .map_err(|source| io_error("read a lower symbolic link during capture", source))?
            .into_os_string();
        let after = symlink_metadata(&lower, "reinspect a lower symbolic link after capture")?;
        if !same_fingerprint(&before, &after) {
            return Err(concurrent_change(path));
        }
        let digest = digest_bytes(target.as_bytes());
        Ok((
            base_record(&before, EntryKind::Symlink, Some(digest)),
            target,
        ))
    }

    fn upper_metadata(&self, path: &Path, node: &NodeRecord) -> ShareFsResult<EntryMetadata> {
        if node.id == ROOT_NODE_ID {
            return lower_metadata(&self.lower, path);
        }
        if node.kind == EntryKind::Directory
            && !node.metadata_changed
            && node.merge_lower
            && !node.opaque
            && let Some(lower) = optional_symlink_metadata(
                &self.lower.join(path),
                "inspect merged lower directory metadata",
            )?
            && entry_kind(&lower, path)? == EntryKind::Directory
        {
            return metadata_value(&lower, path);
        }
        let size = match node.kind {
            EntryKind::File => fs::metadata(self.object_path(node)?)
                .map_err(|source| io_error("inspect an upper file object", source))?
                .len(),
            EntryKind::Directory => 0,
            EntryKind::Symlink => node
                .symlink_target
                .as_ref()
                .ok_or_else(|| Report::new(ShareFsError::CorruptState))?
                .as_bytes()
                .len() as u64,
        };
        Ok(EntryMetadata {
            kind: node.kind,
            size,
            mode: node.mode,
            modified_at: node.modified_at,
        })
    }

    fn ensure_absent(&self, path: &Path) -> ShareFsResult<()> {
        match self.resolve(path) {
            Ok(_) => Err(Report::new(ShareFsError::AlreadyExists {
                path: path.to_owned(),
            })),
            Err(error) if matches!(error.error(), ShareFsError::NotFound { .. }) => Ok(()),
            Err(error) => Err(error),
        }
    }

    fn require_directory(&self, path: &Path) -> ShareFsResult<()> {
        let kind = match self.resolve(path)? {
            ResolvedEntry::Upper(node) => node.kind,
            ResolvedEntry::Lower(lower) => {
                let metadata = symlink_metadata(&lower, "inspect a destination parent")?;
                entry_kind(&metadata, path)?
            }
        };
        if kind != EntryKind::Directory {
            return Err(Report::new(ShareFsError::NotDirectory {
                path: path.to_owned(),
            }));
        }
        Ok(())
    }

    fn collect_changes(
        &self,
        directory: i64,
        path: &Path,
        changes: &mut Vec<ShareChange>,
    ) -> ShareFsResult<()> {
        for entry in self.namespace.entries(directory)? {
            let child_path = path.join(&entry.name);
            match entry.state {
                EntryState::Whiteout => {
                    let base = entry
                        .base
                        .ok_or_else(|| Report::new(ShareFsError::CorruptState))?;
                    changes.push(ShareChange {
                        path: child_path,
                        base: Some(base),
                        proposed: None,
                        opaque: false,
                    });
                }
                EntryState::Present(node_id) => {
                    let node = self.namespace.node(node_id)?;
                    let proposed = self.version_for_node(&node)?;
                    let include = entry
                        .base
                        .as_ref()
                        .is_none_or(|base| node.opaque || base.version != proposed);
                    if include {
                        changes.push(ShareChange {
                            path: child_path.clone(),
                            base: entry.base,
                            proposed: Some(proposed),
                            opaque: node.opaque,
                        });
                    }
                    if node.kind == EntryKind::Directory {
                        self.collect_changes(node.id, &child_path, changes)?;
                    }
                }
            }
        }
        Ok(())
    }

    fn version_for_node(&self, node: &NodeRecord) -> ShareFsResult<EntryVersion> {
        let (size, content_digest) = match node.kind {
            EntryKind::File => {
                let object = self.object_path(node)?;
                let metadata = fs::metadata(&object)
                    .map_err(|source| io_error("inspect a proposed upper file", source))?;
                (metadata.len(), Some(digest_file(&object)?))
            }
            EntryKind::Directory => (0, None),
            EntryKind::Symlink => {
                let target = node
                    .symlink_target
                    .as_ref()
                    .ok_or_else(|| Report::new(ShareFsError::CorruptState))?;
                (
                    target.as_bytes().len() as u64,
                    Some(digest_bytes(target.as_bytes())),
                )
            }
        };
        Ok(EntryVersion {
            kind: node.kind,
            size,
            mode: node.mode,
            content_digest,
        })
    }

    fn start_object(&self) -> ShareFsResult<(String, PathBuf, File)> {
        let object_name = Uuid::new_v4().simple().to_string();
        let staging_path = self.staging.join(format!("{object_name}.tmp"));
        let file = OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(&staging_path)
            .map_err(|source| io_error("create a staged upper object", source))?;
        Ok((object_name, staging_path, file))
    }

    fn create_empty_object(&self) -> ShareFsResult<String> {
        let (object_name, staging_path, file) = self.start_object()?;
        self.commit_object(&staging_path, &object_name, file)?;
        Ok(object_name)
    }

    fn commit_object(
        &self,
        staging_path: &Path,
        object_name: &str,
        mut file: File,
    ) -> ShareFsResult<()> {
        file.flush()
            .map_err(|source| io_error("flush a staged upper object", source))?;
        file.sync_all()
            .map_err(|source| io_error("synchronize a staged upper object", source))?;
        drop(file);
        fs::rename(staging_path, self.objects.join(object_name))
            .map_err(|source| io_error("publish a staged upper object", source))?;
        sync_directory(&self.objects)
    }

    fn object_path(&self, node: &NodeRecord) -> ShareFsResult<PathBuf> {
        node.object_name
            .as_ref()
            .map(|name| self.objects.join(name))
            .ok_or_else(|| Report::new(ShareFsError::CorruptState))
    }

    fn collect_garbage(&mut self) -> ShareFsResult<()> {
        for object in self.namespace.collect_unreachable_objects()? {
            remove_file_if_exists(&self.objects.join(object))?;
        }
        Ok(())
    }

    fn validate_objects(&self) -> ShareFsResult<()> {
        let referenced = self.namespace.referenced_objects()?;
        let mut found = BTreeSet::new();
        for entry in fs::read_dir(&self.objects)
            .map_err(|source| io_error("inventory upper object files", source))?
        {
            let entry =
                entry.map_err(|source| io_error("read an upper object directory entry", source))?;
            let metadata = fs::symlink_metadata(entry.path())
                .map_err(|source| io_error("inspect an upper object", source))?;
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                return Err(Report::new(ShareFsError::CorruptState));
            }
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| Report::new(ShareFsError::CorruptState))?;
            if referenced.contains(&name) {
                found.insert(name);
            } else {
                fs::remove_file(entry.path())
                    .map_err(|source| io_error("remove an orphaned upper object", source))?;
            }
        }
        if found != referenced {
            return Err(Report::new(ShareFsError::CorruptState));
        }
        Ok(())
    }
}

enum ResolvedEntry {
    Upper(NodeRecord),
    Lower(PathBuf),
}

/// Returns the supported kind of one resolved merged entry.
fn resolved_entry_kind(entry: &ResolvedEntry, logical: &Path) -> ShareFsResult<EntryKind> {
    match entry {
        ResolvedEntry::Upper(node) => Ok(node.kind),
        ResolvedEntry::Lower(path) => {
            let metadata = symlink_metadata(path, "inspect a resolved lower entry")?;
            entry_kind(&metadata, logical)
        }
    }
}
