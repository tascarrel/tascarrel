//! Durable records for pending overlay-share approval requests.

use std::fs;
use std::fs::DirBuilder;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::Write as _;
use std::os::unix::fs::DirBuilderExt as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::path::PathBuf;
use std::str::FromStr as _;

use jiff::Timestamp;
use reportify::ErrorExt as _;
use reportify::Report;
use serde::Deserialize;
use serde::Serialize;
use tascarrel_api::ids::ShareOverlayApprovalId;
use tascarrel_api::types::pods::PodId;
use tascarrel_api::types::shares as api;
use thiserror::Error;

const RECORD_VERSION: u32 = 1;
const MAX_RECORD_BYTES: u64 = 64 * 1024 * 1024;
const MAX_RECORDS: usize = 10_000;

/// Exact submitted revision retained until a host decision.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StoredShareOverlayApproval {
    version: u32,
    /// Stable request identifier.
    pub(crate) id: ShareOverlayApprovalId,
    /// Workspace which owns the pod and configured share.
    pub(crate) workspace: String,
    /// Pod which submitted the revision.
    pub(crate) pod_id: String,
    /// Configured overlay share name.
    pub(crate) share: String,
    /// Time at which hostd inspected the exact revision.
    pub(crate) created_at: Timestamp,
    /// Exact upper revision submitted for approval.
    pub(crate) revision: String,
    /// Bounded review summary captured with the revision.
    pub(crate) changes: Vec<api::ShareOverlayChange>,
}

impl StoredShareOverlayApproval {
    /// Creates one current-version pending approval record.
    pub(crate) fn new(
        id: ShareOverlayApprovalId,
        workspace: String,
        pod_id: String,
        share: String,
        revision: String,
        changes: Vec<api::ShareOverlayChange>,
    ) -> Self {
        Self {
            version: RECORD_VERSION,
            id,
            workspace,
            pod_id,
            share,
            created_at: Timestamp::now(),
            revision,
            changes,
        }
    }

    fn validate(&self) -> Result<(), Report<ShareOverlayApprovalStorageError>> {
        if self.version != RECORD_VERSION {
            return Err(
                ShareOverlayApprovalStorageError::UnsupportedVersion(self.version).report(),
            );
        }
        if tascarrel_protocol::WorkspaceName::new(&self.workspace).is_err()
            || PodId::from_str(&self.pod_id).is_err()
            || !tascarrel_protocol::valid_workspace_share_name(&self.share)
            || self.revision.len() != 64
            || !self
                .revision
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
            || self.changes.len() > tascarrel_protocol::MAX_SHARE_OVERLAY_CHANGES
        {
            return Err(ShareOverlayApprovalStorageError::InvalidRecord.report());
        }
        Ok(())
    }
}

/// Private filesystem storage for pending overlay approvals.
#[derive(Clone, Debug)]
pub(crate) struct ShareOverlayApprovalStorage {
    root: PathBuf,
}

impl ShareOverlayApprovalStorage {
    /// Opens or creates the private approval directory.
    pub(crate) fn open(
        root: impl Into<PathBuf>,
    ) -> Result<Self, Report<ShareOverlayApprovalStorageError>> {
        let root = root.into();
        ensure_private_directory(&root)?;
        Ok(Self { root })
    }

    /// Loads every complete pending approval record.
    pub(crate) fn load(
        &self,
    ) -> Result<Vec<StoredShareOverlayApproval>, Report<ShareOverlayApprovalStorageError>> {
        let mut records = Vec::new();
        for entry in
            fs::read_dir(&self.root).map_err(|error| io("list overlay approval records", error))?
        {
            let entry =
                entry.map_err(|error| io("read overlay approval directory entry", error))?;
            let path = entry.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
                continue;
            }
            if records.len() == MAX_RECORDS {
                return Err(ShareOverlayApprovalStorageError::RecordLimit.report());
            }
            let record = read_record(&path)?;
            record.validate()?;
            if path.file_stem().and_then(|stem| stem.to_str()) != Some(record.id.0.as_ref()) {
                return Err(ShareOverlayApprovalStorageError::InvalidRecord.report());
            }
            records.push(record);
        }
        records.sort_by(|left, right| {
            left.created_at
                .cmp(&right.created_at)
                .then_with(|| left.id.cmp(&right.id))
        });
        Ok(records)
    }

    /// Atomically publishes one new pending approval record.
    pub(crate) fn create(
        &self,
        record: &StoredShareOverlayApproval,
    ) -> Result<(), Report<ShareOverlayApprovalStorageError>> {
        record.validate()?;
        let final_path = self.record_path(&record.id);
        if final_path.exists() {
            return Err(ShareOverlayApprovalStorageError::AlreadyExists.report());
        }
        let temporary = self
            .root
            .join(format!(".approval-{}.tmp", uuid::Uuid::new_v4()));
        let bytes = serde_json::to_vec_pretty(record)
            .map_err(|error| encode("encode overlay approval record", error))?;
        if bytes.len() as u64 > MAX_RECORD_BYTES {
            return Err(ShareOverlayApprovalStorageError::RecordTooLarge.report());
        }
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .custom_flags(nix::libc::O_CLOEXEC | nix::libc::O_NOFOLLOW)
            .open(&temporary)
            .map_err(|error| io("create overlay approval record", error))?;
        file.write_all(&bytes)
            .map_err(|error| io("write overlay approval record", error))?;
        file.sync_all()
            .map_err(|error| io("sync overlay approval record", error))?;
        fs::rename(&temporary, &final_path)
            .map_err(|error| io("publish overlay approval record", error))?;
        sync_directory(&self.root)
    }

    /// Durably removes one resolved or withdrawn approval record.
    pub(crate) fn remove(
        &self,
        id: &ShareOverlayApprovalId,
    ) -> Result<(), Report<ShareOverlayApprovalStorageError>> {
        fs::remove_file(self.record_path(id))
            .map_err(|error| io("remove overlay approval record", error))?;
        sync_directory(&self.root)
    }

    fn record_path(&self, id: &ShareOverlayApprovalId) -> PathBuf {
        self.root.join(format!("{}.json", id.0))
    }
}

/// Failure to open or mutate durable overlay approval storage.
#[derive(Debug, Error)]
pub(crate) enum ShareOverlayApprovalStorageError {
    /// A record was written by an unsupported future schema version.
    #[error("overlay approval storage has unsupported record version {0}")]
    UnsupportedVersion(u32),
    /// A record does not satisfy the durable storage invariants.
    #[error("overlay approval storage contains an invalid record")]
    InvalidRecord,
    /// The configured maximum number of pending records was exceeded.
    #[error("overlay approval storage exceeds its record limit")]
    RecordLimit,
    /// A serialized record exceeds its bounded size.
    #[error("overlay approval record exceeds its size limit")]
    RecordTooLarge,
    /// A generated approval identifier unexpectedly already exists.
    #[error("overlay approval record already exists")]
    AlreadyExists,
    /// A persistent storage path is not a real directory.
    #[error("overlay approval storage path is unsafe: {0}")]
    UnsafePath(PathBuf),
    /// A persistent filesystem operation failed.
    #[error("overlay approval storage I/O failed")]
    Io,
    /// A persistent record could not be encoded or decoded.
    #[error("overlay approval storage contains invalid JSON")]
    InvalidJson,
}

/// Creates the storage directory and rejects symlink or non-directory roots.
fn ensure_private_directory(path: &Path) -> Result<(), Report<ShareOverlayApprovalStorageError>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(ShareOverlayApprovalStorageError::UnsafePath(path.to_owned()).report());
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut builder = DirBuilder::new();
            builder.recursive(true).mode(0o700);
            builder
                .create(path)
                .map_err(|error| io("create overlay approval storage", error))?;
        }
        Err(error) => return Err(io("inspect overlay approval storage", error)),
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|error| io("secure overlay approval storage", error))
}

/// Reads one bounded regular-file record without following a final symlink.
fn read_record(
    path: &Path,
) -> Result<StoredShareOverlayApproval, Report<ShareOverlayApprovalStorageError>> {
    let metadata =
        fs::symlink_metadata(path).map_err(|error| io("inspect overlay approval record", error))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(ShareOverlayApprovalStorageError::InvalidRecord.report());
    }
    if metadata.len() > MAX_RECORD_BYTES {
        return Err(ShareOverlayApprovalStorageError::RecordTooLarge.report());
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_CLOEXEC | nix::libc::O_NOFOLLOW)
        .open(path)
        .map_err(|error| io("open overlay approval record", error))?;
    serde_json::from_reader(file).map_err(|error| encode("decode overlay approval record", error))
}

/// Flushes directory entry changes after publishing or removing a record.
fn sync_directory(path: &Path) -> Result<(), Report<ShareOverlayApprovalStorageError>> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| io("sync overlay approval directory", error))
}

fn io(action: &'static str, error: std::io::Error) -> Report<ShareOverlayApprovalStorageError> {
    error
        .escalate(ShareOverlayApprovalStorageError::Io)
        .message(action)
}

fn encode(
    action: &'static str,
    error: serde_json::Error,
) -> Report<ShareOverlayApprovalStorageError> {
    error
        .escalate(ShareOverlayApprovalStorageError::InvalidJson)
        .message(action)
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    /// Verifies pending approval records survive reopening and disappear after
    /// removal.
    #[test]
    fn records_round_trip_durably() {
        let root = tempdir().unwrap();
        let storage = ShareOverlayApprovalStorage::open(root.path()).unwrap();
        let record = StoredShareOverlayApproval::new(
            ShareOverlayApprovalId::generate(),
            "example".to_owned(),
            PodId::generate().0.to_string(),
            "source".to_owned(),
            "a".repeat(64),
            Vec::new(),
        );
        storage.create(&record).unwrap();

        let reopened = ShareOverlayApprovalStorage::open(root.path()).unwrap();
        let records = reopened.load().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].id, record.id);
        assert_eq!(records[0].revision, record.revision);

        reopened.remove(&record.id).unwrap();
        assert!(reopened.load().unwrap().is_empty());
    }
}
