//! Durable repository push status shared by transport and control-plane
//! services.
//!
//! [`RepositoryPushStatusStore`] retains one small, pod-scoped status after
//! receive-pack closes. Pending approvals transition in place so a pod-side
//! subscriber can wait for a definitive publication result.

use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::path::PathBuf;
use std::str::FromStr as _;
use std::time::Duration;

use reportify::ErrorExt as _;
use reportify::Report;
use serde::Deserialize;
use serde::Serialize;
use tascarrel_api::ids::RepositoryApprovalId;
use tascarrel_api::ids::RepositoryPushId;
use tascarrel_api::types::pods::PodId;
use thiserror::Error;

/// Durable current status retained for one pod push.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RepositoryPushStatus {
    version: u32,
    /// Push operation which produced this result.
    pub(crate) id: RepositoryPushId,
    /// Pod allowed to consume the result.
    pub(crate) pod_id: String,
    /// Current policy, approval, or publication state.
    pub(crate) state: RepositoryPushState,
}

impl RepositoryPushStatus {
    /// Creates a current-version push status.
    pub(crate) fn new(id: RepositoryPushId, pod_id: String, state: RepositoryPushState) -> Self {
        Self {
            version: STATUS_RECORD_VERSION,
            id,
            pod_id,
            state,
        }
    }

    fn validate(&self) -> Result<(), Report<RepositoryPushStatusStoreError>> {
        if self.version != STATUS_RECORD_VERSION {
            return Err(RepositoryPushStatusStoreError::UnsupportedVersion(self.version).report());
        }
        if PodId::from_str(&self.pod_id).is_err() || !self.state.is_valid() {
            return Err(RepositoryPushStatusStoreError::InvalidRecord.report());
        }
        Ok(())
    }
}

/// Policy, approval, and publication state stored independently of the API.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "status", content = "value", rename_all = "snake_case")]
pub(crate) enum RepositoryPushState {
    /// Every update was published upstream.
    Published,
    /// Every update was retained in one approval.
    ApprovalRequired(RepositoryApprovalId),
    /// Configured policy rejected at least one update.
    Denied(String),
    /// The pending approval was rejected.
    Rejected,
    /// Upstream publication or terminal processing failed.
    Failed(String),
}

impl RepositoryPushState {
    fn is_valid(&self) -> bool {
        match self {
            Self::Published | Self::ApprovalRequired(_) | Self::Rejected => true,
            Self::Denied(message) | Self::Failed(message) => {
                !message.is_empty() && message.len() <= 4096 && !message.contains('\0')
            }
        }
    }

    const fn is_terminal(&self) -> bool {
        !matches!(self, Self::ApprovalRequired(_))
    }
}

/// Workspace-scoped persistence for observable push statuses.
#[derive(Clone, Debug)]
pub(crate) struct RepositoryPushStatusStore {
    directory: PathBuf,
}

impl RepositoryPushStatusStore {
    /// Opens or creates the private status directory below a workspace cache.
    pub(crate) fn open(
        workspace_cache: &Path,
    ) -> Result<Self, Report<RepositoryPushStatusStoreError>> {
        let directory = workspace_cache.join("push-statuses");
        if !directory.exists() {
            fs::create_dir_all(&directory).map_err(|source| {
                source
                    .escalate(RepositoryPushStatusStoreError::Io)
                    .message("create repository push status directory")
            })?;
            fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).map_err(
                |source| {
                    source
                        .escalate(RepositoryPushStatusStoreError::Io)
                        .message("set repository push status directory permissions")
                },
            )?;
        }
        let metadata = fs::symlink_metadata(&directory).map_err(|source| {
            source
                .escalate(RepositoryPushStatusStoreError::Io)
                .message("inspect repository push status directory")
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(RepositoryPushStatusStoreError::UnsafePath(directory).report());
        }
        Ok(Self { directory })
    }

    /// Atomically publishes the initial status of one push.
    pub(crate) fn create(
        &self,
        status: &RepositoryPushStatus,
    ) -> Result<(), Report<RepositoryPushStatusStoreError>> {
        status.validate()?;
        self.prune_and_count()?;
        let final_path = self.record_path(&status.id);
        if final_path.exists() {
            return Err(RepositoryPushStatusStoreError::AlreadyExists.report());
        }
        self.write(status)
    }

    /// Loads the current status when it belongs to `pod_id`.
    pub(crate) fn read(
        &self,
        id: &RepositoryPushId,
        pod_id: &str,
    ) -> Result<RepositoryPushStatus, Report<RepositoryPushStatusStoreError>> {
        let status = self.read_path(&self.record_path(id))?;
        if status.pod_id != pod_id {
            return Err(RepositoryPushStatusStoreError::NotFound.report());
        }
        Ok(status)
    }

    /// Atomically transitions a pending approval to a terminal push state.
    pub(crate) fn transition(
        &self,
        id: &RepositoryPushId,
        state: RepositoryPushState,
    ) -> Result<(), Report<RepositoryPushStatusStoreError>> {
        let mut status = self.read_path(&self.record_path(id))?;
        if status.state == state {
            return Ok(());
        }
        if !matches!(status.state, RepositoryPushState::ApprovalRequired(_)) || !state.is_terminal()
        {
            return Err(RepositoryPushStatusStoreError::InvalidTransition.report());
        }
        status.state = state;
        status.validate()?;
        self.write(&status)
    }

    fn write(
        &self,
        status: &RepositoryPushStatus,
    ) -> Result<(), Report<RepositoryPushStatusStoreError>> {
        let final_path = self.record_path(&status.id);
        let temporary =
            self.directory
                .join(format!(".{}.{}.tmp", status.id.0, uuid::Uuid::new_v4()));
        let bytes = serde_json::to_vec(status).map_err(|source| {
            source
                .escalate(RepositoryPushStatusStoreError::InvalidRecord)
                .message("encode repository push status")
        })?;
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_STATUS_RECORD_BYTES {
            return Err(RepositoryPushStatusStoreError::RecordTooLarge.report());
        }
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&temporary)
            .map_err(|source| {
                source
                    .escalate(RepositoryPushStatusStoreError::Io)
                    .message("create temporary repository push status")
            })?;
        let result = (|| {
            file.write_all(&bytes).map_err(|source| {
                source
                    .escalate(RepositoryPushStatusStoreError::Io)
                    .message("write repository push status")
            })?;
            file.sync_all().map_err(|source| {
                source
                    .escalate(RepositoryPushStatusStoreError::Io)
                    .message("sync repository push status")
            })?;
            fs::rename(&temporary, &final_path).map_err(|source| {
                source
                    .escalate(RepositoryPushStatusStoreError::Io)
                    .message("publish repository push status")
            })?;
            sync_directory(&self.directory)
        })();
        if result.is_err()
            && let Err(source) = fs::remove_file(&temporary)
            && source.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(path = %temporary.display(), %source, "could not remove temporary push status");
        }
        result
    }

    fn read_path(
        &self,
        path: &Path,
    ) -> Result<RepositoryPushStatus, Report<RepositoryPushStatusStoreError>> {
        let metadata = fs::symlink_metadata(path).map_err(|source| {
            if source.kind() == std::io::ErrorKind::NotFound {
                RepositoryPushStatusStoreError::NotFound.report()
            } else {
                source
                    .escalate(RepositoryPushStatusStoreError::Io)
                    .message("inspect repository push status")
            }
        })?;
        if metadata.file_type().is_symlink()
            || !metadata.is_file()
            || metadata.len() > MAX_STATUS_RECORD_BYTES
        {
            return Err(RepositoryPushStatusStoreError::UnsafePath(path.to_owned()).report());
        }
        let bytes = fs::read(path).map_err(|source| {
            source
                .escalate(RepositoryPushStatusStoreError::Io)
                .message("read repository push status")
        })?;
        let status: RepositoryPushStatus = serde_json::from_slice(&bytes).map_err(|source| {
            source
                .escalate(RepositoryPushStatusStoreError::InvalidRecord)
                .message("decode repository push status")
        })?;
        status.validate()?;
        if self.record_path(&status.id) != path {
            return Err(RepositoryPushStatusStoreError::InvalidRecord.report());
        }
        Ok(status)
    }

    fn prune_and_count(&self) -> Result<(), Report<RepositoryPushStatusStoreError>> {
        let mut count = 0;
        let mut removed = false;
        for entry in fs::read_dir(&self.directory).map_err(|source| {
            source
                .escalate(RepositoryPushStatusStoreError::Io)
                .message("list repository push statuses")
        })? {
            let entry = entry.map_err(|source| {
                source
                    .escalate(RepositoryPushStatusStoreError::Io)
                    .message("read repository push status directory entry")
            })?;
            let path = entry.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
                continue;
            }
            let metadata = fs::symlink_metadata(&path).map_err(|source| {
                source
                    .escalate(RepositoryPushStatusStoreError::Io)
                    .message("inspect retained repository push status")
            })?;
            let old = !metadata.file_type().is_symlink()
                && metadata.is_file()
                && metadata
                    .modified()
                    .ok()
                    .and_then(|modified| modified.elapsed().ok())
                    .is_some_and(|age| age >= TERMINAL_STATUS_RETENTION);
            let expired = old && self.read_path(&path)?.state.is_terminal();
            if expired {
                if let Err(source) = fs::remove_file(&path) {
                    tracing::warn!(path = %path.display(), %source, "could not prune expired push status");
                    count += 1;
                } else {
                    removed = true;
                }
            } else {
                count += 1;
            }
        }
        if removed {
            sync_directory(&self.directory)?;
        }
        if count >= MAX_STATUS_RECORDS {
            return Err(RepositoryPushStatusStoreError::RecordLimit.report());
        }
        Ok(())
    }

    fn record_path(&self, id: &RepositoryPushId) -> PathBuf {
        self.directory.join(format!("{}.json", id.0))
    }
}

/// Caller-relevant durable push-status store failures.
#[derive(Debug, Error)]
pub(crate) enum RepositoryPushStatusStoreError {
    /// Push status is absent or belongs to another pod.
    #[error("repository push status does not exist")]
    NotFound,
    /// Push identifier already has an initial status.
    #[error("repository push status already exists")]
    AlreadyExists,
    /// The stored state cannot make the requested transition.
    #[error("repository push status transition is invalid")]
    InvalidTransition,
    /// Stored status is malformed.
    #[error("repository push status is invalid")]
    InvalidRecord,
    /// Stored status uses an unsupported format version.
    #[error("repository push status version {0} is unsupported")]
    UnsupportedVersion(u32),
    /// Status directory or record is not a safe real path.
    #[error("repository push status path is unsafe: {0}")]
    UnsafePath(PathBuf),
    /// Workspace contains too many retained statuses.
    #[error("repository push status limit was exceeded")]
    RecordLimit,
    /// Encoded status exceeds its configured bound.
    #[error("repository push status exceeds its size limit")]
    RecordTooLarge,
    /// Filesystem operation failed.
    #[error("repository push status storage failed")]
    Io,
}

const STATUS_RECORD_VERSION: u32 = 1;
const MAX_STATUS_RECORDS: usize = 10_000;
const MAX_STATUS_RECORD_BYTES: u64 = 16 * 1024;
const TERMINAL_STATUS_RETENTION: Duration = Duration::from_hours(24);

fn sync_directory(path: &Path) -> Result<(), Report<RepositoryPushStatusStoreError>> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| {
            source
                .escalate(RepositoryPushStatusStoreError::Io)
                .message("sync repository push status directory")
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pending status survives reopening and transitions to a terminal state.
    #[test]
    fn push_status_transitions_durably() {
        let temporary = tempfile::tempdir().expect("create temporary directory");
        let store = RepositoryPushStatusStore::open(temporary.path()).expect("open status store");
        let pod_id = PodId::generate().0.to_string();
        let status = RepositoryPushStatus::new(
            RepositoryPushId::generate(),
            pod_id.clone(),
            RepositoryPushState::ApprovalRequired(RepositoryApprovalId::generate()),
        );
        store.create(&status).expect("create status");

        let reopened =
            RepositoryPushStatusStore::open(temporary.path()).expect("reopen status store");
        assert!(matches!(
            reopened
                .read(&status.id, &pod_id)
                .expect("read status")
                .state,
            RepositoryPushState::ApprovalRequired(_)
        ));
        reopened
            .transition(&status.id, RepositoryPushState::Published)
            .expect("publish status");
        assert!(matches!(
            reopened
                .read(&status.id, &pod_id)
                .expect("read status")
                .state,
            RepositoryPushState::Published
        ));
    }

    /// A different pod cannot observe another pod's push status.
    #[test]
    fn push_statuses_are_pod_scoped() {
        let temporary = tempfile::tempdir().expect("create temporary directory");
        let store = RepositoryPushStatusStore::open(temporary.path()).expect("open status store");
        let status = RepositoryPushStatus::new(
            RepositoryPushId::generate(),
            PodId::generate().0.to_string(),
            RepositoryPushState::Denied("push denied by Git policy".to_owned()),
        );
        store.create(&status).expect("create status");

        let other_pod = PodId::generate().0.to_string();
        assert!(store.read(&status.id, &other_pod).is_err());
        assert!(store.read(&status.id, &status.pod_id).is_ok());
    }
}
