//! Durable host-owned repository publication approval records and storage.
//!
//! [`RepositoryApprovalStore`] persists exact ref updates below one
//! workspace-scoped cache root. Git objects remain retained by refs in the
//! corresponding bare store; records contain no upstream credentials.

use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::str::FromStr as _;

use jiff::Timestamp;
use reportify::ErrorExt as _;
use reportify::Report;
use serde::Deserialize;
use serde::Serialize;
use tascarrel_api::ids::RepositoryApprovalId;
use tascarrel_api::ids::RepositoryPushId;
use tascarrel_api::types::pods::PodId;
use tascarrel_git::ObjectId;
use tascarrel_git::ReceiveNamespace;
use tascarrel_git::ReferenceName;
use thiserror::Error;

const APPROVAL_RECORD_VERSION: u32 = 1;
const MAX_APPROVAL_RECORDS: usize = 10_000;
const MAX_APPROVAL_RECORD_BYTES: u64 = 1024 * 1024;

/// Durable approval request for one atomic branch and tag publication.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RepositoryApproval {
    version: u32,
    /// Stable request identifier.
    pub(crate) id: RepositoryApprovalId,
    /// Pod which proposed the updates.
    pub(crate) pod_id: String,
    /// Configured repository path below `/workspace`.
    pub(crate) path: String,
    /// Credential-free identity of the workspace repository object store.
    pub(crate) repository_id: String,
    /// Display-safe upstream source captured when the request was created.
    pub(crate) source: String,
    /// Time at which the objects and refs were retained.
    pub(crate) created_at: Timestamp,
    /// Exact ref updates protected by their observed upstream values.
    pub(crate) updates: Vec<RepositoryApprovalUpdate>,
    /// Originating push whose subscriber waits for this approval, when any.
    pub(crate) push_id: Option<RepositoryPushId>,
    /// Receive-pack namespace to remove after resolution, when applicable.
    pub(crate) receive_namespace: Option<String>,
    /// Whether a user postponed the automatic approval overlay.
    #[serde(default)]
    pub(crate) postponed: bool,
    /// Most recent failed background publication attempt.
    pub(crate) last_error: Option<String>,
    /// Whether a durable background-publication claim currently exists.
    #[serde(skip)]
    pub(crate) publishing: bool,
}

impl RepositoryApproval {
    /// Creates a current-version durable approval record.
    #[allow(
        clippy::too_many_arguments,
        reason = "each argument maps directly to one immutable approval field"
    )]
    pub(crate) fn new(
        id: RepositoryApprovalId,
        pod_id: String,
        path: String,
        repository_id: String,
        source: String,
        updates: Vec<RepositoryApprovalUpdate>,
        push_id: Option<RepositoryPushId>,
        receive_namespace: Option<String>,
    ) -> Self {
        Self {
            version: APPROVAL_RECORD_VERSION,
            id,
            pod_id,
            path,
            repository_id,
            source,
            created_at: Timestamp::now(),
            updates,
            push_id,
            receive_namespace,
            postponed: false,
            last_error: None,
            publishing: false,
        }
    }

    fn validate(&self) -> Result<(), Report<RepositoryApprovalStoreError>> {
        let path = Path::new(&self.path);
        if self.version != APPROVAL_RECORD_VERSION {
            return Err(RepositoryApprovalStoreError::UnsupportedVersion(self.version).report());
        }
        if self.path.is_empty()
            || path.is_absolute()
            || path
                .components()
                .any(|component| !matches!(component, Component::Normal(_)))
            || PodId::from_str(&self.pod_id).is_err()
            || self.source.is_empty()
            || self.source.len() > 4096
            || self.source.chars().any(char::is_control)
            || self.repository_id.len() != 64
            || !self
                .repository_id
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
            || self.updates.is_empty()
            || self
                .last_error
                .as_deref()
                .is_some_and(|error| error.is_empty() || error.len() > 4096 || error.contains('\0'))
        {
            return Err(RepositoryApprovalStoreError::InvalidRecord.report());
        }
        if self
            .receive_namespace
            .as_deref()
            .is_some_and(|namespace| ReceiveNamespace::new(namespace).is_err())
            || self.updates.iter().any(|update| {
                let Ok(destination) = ReferenceName::new(&update.destination) else {
                    return true;
                };
                ReferenceName::new(&update.source).is_err()
                    || (!destination.is_branch() && !destination.is_tag())
                    || update
                        .expected
                        .as_deref()
                        .is_some_and(|object| ObjectId::new(object).is_err())
                    || ObjectId::new(&update.proposed).is_err()
            })
        {
            return Err(RepositoryApprovalStoreError::InvalidRecord.report());
        }
        Ok(())
    }
}

/// One exact retained ref update within an approval request.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RepositoryApprovalUpdate {
    /// Retained source ref in the workspace object store.
    pub(crate) source: String,
    /// Upstream branch or tag destination.
    pub(crate) destination: String,
    /// Upstream object observed when the request was staged.
    pub(crate) expected: Option<String>,
    /// Exact proposed object retained by `source`.
    pub(crate) proposed: String,
    /// Whether approval authorizes a non-fast-forward branch or tag rewrite.
    pub(crate) allow_rewrite: bool,
}

/// Workspace-scoped persistence for pending repository approvals.
#[derive(Clone, Debug)]
pub(crate) struct RepositoryApprovalStore {
    directory: PathBuf,
}

impl RepositoryApprovalStore {
    /// Opens or creates the private approval directory below a workspace cache.
    pub(crate) fn open(
        workspace_cache: &Path,
    ) -> Result<Self, Report<RepositoryApprovalStoreError>> {
        let directory = workspace_cache.join("approvals");
        if !directory.exists() {
            fs::create_dir_all(&directory).map_err(|source| {
                source
                    .escalate(RepositoryApprovalStoreError::Io)
                    .message("create repository approval directory")
            })?;
            fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).map_err(
                |source| {
                    source
                        .escalate(RepositoryApprovalStoreError::Io)
                        .message("set repository approval directory permissions")
                },
            )?;
        }
        let metadata = fs::symlink_metadata(&directory).map_err(|source| {
            source
                .escalate(RepositoryApprovalStoreError::Io)
                .message("inspect repository approval directory")
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(RepositoryApprovalStoreError::UnsafePath(directory).report());
        }
        Ok(Self { directory })
    }

    /// Lists every complete pending approval record.
    pub(crate) fn list(
        &self,
    ) -> Result<Vec<RepositoryApproval>, Report<RepositoryApprovalStoreError>> {
        let mut approvals = Vec::new();
        for entry in fs::read_dir(&self.directory).map_err(|source| {
            source
                .escalate(RepositoryApprovalStoreError::Io)
                .message("list repository approval records")
        })? {
            let entry = entry.map_err(|source| {
                source
                    .escalate(RepositoryApprovalStoreError::Io)
                    .message("read repository approval directory entry")
            })?;
            let path = entry.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
                continue;
            }
            if approvals.len() == MAX_APPROVAL_RECORDS {
                return Err(RepositoryApprovalStoreError::RecordLimit.report());
            }
            let mut approval = self.read_path(&path)?;
            approval.publishing = self.claim_exists(&approval.id)?;
            approvals.push(approval);
        }
        approvals.sort_by(|left, right| {
            left.created_at
                .cmp(&right.created_at)
                .then_with(|| left.id.cmp(&right.id))
        });
        Ok(approvals)
    }

    /// Loads one pending approval record.
    pub(crate) fn read(
        &self,
        id: &RepositoryApprovalId,
    ) -> Result<RepositoryApproval, Report<RepositoryApprovalStoreError>> {
        let path = self.record_path(id);
        match fs::symlink_metadata(&path) {
            Ok(_) => {
                let mut approval = self.read_path(&path)?;
                approval.publishing = self.claim_exists(id)?;
                Ok(approval)
            }
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                Err(RepositoryApprovalStoreError::NotFound.report())
            }
            Err(source) => Err(source
                .escalate(RepositoryApprovalStoreError::Io)
                .message("inspect repository approval record")),
        }
    }

    /// Atomically publishes one pending approval record.
    pub(crate) fn create(
        &self,
        approval: &RepositoryApproval,
    ) -> Result<(), Report<RepositoryApprovalStoreError>> {
        approval.validate()?;
        let final_path = self.record_path(&approval.id);
        if final_path.exists() || self.claim_path(&approval.id).exists() {
            return Err(RepositoryApprovalStoreError::AlreadyExists.report());
        }
        self.write(approval)
    }

    /// Durably suppresses the automatic overlay for one pending approval.
    pub(crate) fn postpone(
        &self,
        id: &RepositoryApprovalId,
    ) -> Result<(), Report<RepositoryApprovalStoreError>> {
        let mut approval = self.read(id)?;
        if approval.publishing || approval.last_error.is_some() {
            return Err(RepositoryApprovalStoreError::NotPending.report());
        }
        if approval.postponed {
            return Ok(());
        }
        approval.postponed = true;
        self.write(&approval)
    }

    /// Durably claims one approval for a background publication.
    pub(crate) fn claim(
        &self,
        id: &RepositoryApprovalId,
    ) -> Result<Option<RepositoryApproval>, Report<RepositoryApprovalStoreError>> {
        let mut approval = self.read(id)?;
        if approval.publishing {
            return Ok(None);
        }
        let claim_path = self.claim_path(id);
        let claim = match OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&claim_path)
        {
            Ok(claim) => claim,
            Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => {
                if self.read(id)?.publishing {
                    return Ok(None);
                }
                return Err(RepositoryApprovalStoreError::AlreadyPublishing.report());
            }
            Err(source) => {
                return Err(source
                    .escalate(RepositoryApprovalStoreError::Io)
                    .message("claim repository approval publication"));
            }
        };
        claim.sync_all().map_err(|source| {
            source
                .escalate(RepositoryApprovalStoreError::Io)
                .message("sync repository approval publication claim")
        })?;
        sync_directory(&self.directory)?;

        approval.last_error = None;
        approval.publishing = true;
        if let Err(report) = self.write(&approval) {
            if let Err(source) = fs::remove_file(&claim_path) {
                tracing::warn!(path = %claim_path.display(), %source, "could not roll back approval publication claim");
            }
            return Err(report);
        }
        Ok(Some(approval))
    }

    /// Lists approvals durably claimed for background publication.
    pub(crate) fn claimed(
        &self,
    ) -> Result<Vec<RepositoryApproval>, Report<RepositoryApprovalStoreError>> {
        Ok(self
            .list()?
            .into_iter()
            .filter(|approval| approval.publishing)
            .collect())
    }

    /// Releases a failed background claim and records its bounded diagnostic.
    pub(crate) fn fail_claim(
        &self,
        id: &RepositoryApprovalId,
        error: String,
    ) -> Result<(), Report<RepositoryApprovalStoreError>> {
        if error.is_empty() || error.len() > 4096 || error.contains('\0') {
            return Err(RepositoryApprovalStoreError::InvalidRecord.report());
        }
        if !self.claim_exists(id)? {
            return Err(RepositoryApprovalStoreError::NotPublishing.report());
        }
        let mut approval = self.read_path(&self.record_path(id))?;
        approval.last_error = Some(error);
        self.write(&approval)?;
        self.remove_claim(id)
    }

    /// Removes a successfully published approval and its durable claim.
    pub(crate) fn complete_claim(
        &self,
        id: &RepositoryApprovalId,
    ) -> Result<(), Report<RepositoryApprovalStoreError>> {
        if !self.claim_exists(id)? {
            return Err(RepositoryApprovalStoreError::NotPublishing.report());
        }
        self.remove_record(id)?;
        self.remove_claim(id)
    }

    fn write(
        &self,
        approval: &RepositoryApproval,
    ) -> Result<(), Report<RepositoryApprovalStoreError>> {
        let final_path = self.record_path(&approval.id);
        let temporary =
            self.directory
                .join(format!(".{}.{}.tmp", approval.id.0, uuid::Uuid::new_v4()));
        let bytes = serde_json::to_vec(approval).map_err(|source| {
            source
                .escalate(RepositoryApprovalStoreError::InvalidRecord)
                .message("encode repository approval record")
        })?;
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_APPROVAL_RECORD_BYTES {
            return Err(RepositoryApprovalStoreError::RecordTooLarge.report());
        }
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&temporary)
            .map_err(|source| {
                source
                    .escalate(RepositoryApprovalStoreError::Io)
                    .message("create temporary repository approval record")
            })?;
        let result = (|| {
            file.write_all(&bytes).map_err(|source| {
                source
                    .escalate(RepositoryApprovalStoreError::Io)
                    .message("write repository approval record")
            })?;
            file.sync_all().map_err(|source| {
                source
                    .escalate(RepositoryApprovalStoreError::Io)
                    .message("sync repository approval record")
            })?;
            fs::rename(&temporary, &final_path).map_err(|source| {
                source
                    .escalate(RepositoryApprovalStoreError::Io)
                    .message("publish repository approval record")
            })?;
            sync_directory(&self.directory)
        })();
        if result.is_err()
            && let Err(source) = fs::remove_file(&temporary)
            && source.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(path = %temporary.display(), %source, "could not remove temporary approval record");
        }
        result
    }

    /// Removes one resolved approval record durably.
    pub(crate) fn remove(
        &self,
        id: &RepositoryApprovalId,
    ) -> Result<(), Report<RepositoryApprovalStoreError>> {
        if self.claim_exists(id)? {
            return Err(RepositoryApprovalStoreError::AlreadyPublishing.report());
        }
        self.remove_record(id)?;
        sync_directory(&self.directory)
    }

    fn remove_record(
        &self,
        id: &RepositoryApprovalId,
    ) -> Result<(), Report<RepositoryApprovalStoreError>> {
        let path = self.record_path(id);
        let metadata = fs::symlink_metadata(&path).map_err(|source| {
            if source.kind() == std::io::ErrorKind::NotFound {
                RepositoryApprovalStoreError::NotFound.report()
            } else {
                source
                    .escalate(RepositoryApprovalStoreError::Io)
                    .message("inspect resolved repository approval record")
            }
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(RepositoryApprovalStoreError::UnsafePath(path).report());
        }
        fs::remove_file(&path).map_err(|source| {
            source
                .escalate(RepositoryApprovalStoreError::Io)
                .message("remove resolved repository approval record")
        })
    }

    fn remove_claim(
        &self,
        id: &RepositoryApprovalId,
    ) -> Result<(), Report<RepositoryApprovalStoreError>> {
        let path = self.claim_path(id);
        let metadata = fs::symlink_metadata(&path).map_err(|source| {
            if source.kind() == std::io::ErrorKind::NotFound {
                RepositoryApprovalStoreError::NotPublishing.report()
            } else {
                source
                    .escalate(RepositoryApprovalStoreError::Io)
                    .message("inspect repository approval publication claim")
            }
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(RepositoryApprovalStoreError::UnsafePath(path).report());
        }
        fs::remove_file(&path).map_err(|source| {
            source
                .escalate(RepositoryApprovalStoreError::Io)
                .message("remove repository approval publication claim")
        })?;
        sync_directory(&self.directory)
    }

    fn claim_exists(
        &self,
        id: &RepositoryApprovalId,
    ) -> Result<bool, Report<RepositoryApprovalStoreError>> {
        let path = self.claim_path(id);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => Ok(true),
            Ok(_) => Err(RepositoryApprovalStoreError::UnsafePath(path).report()),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(source) => Err(source
                .escalate(RepositoryApprovalStoreError::Io)
                .message("inspect repository approval publication claim")),
        }
    }

    fn read_path(
        &self,
        path: &Path,
    ) -> Result<RepositoryApproval, Report<RepositoryApprovalStoreError>> {
        let metadata = fs::symlink_metadata(path).map_err(|source| {
            source
                .escalate(RepositoryApprovalStoreError::Io)
                .message("inspect repository approval record")
        })?;
        if metadata.file_type().is_symlink()
            || !metadata.is_file()
            || metadata.len() > MAX_APPROVAL_RECORD_BYTES
        {
            return Err(RepositoryApprovalStoreError::UnsafePath(path.to_owned()).report());
        }
        let bytes = fs::read(path).map_err(|source| {
            source
                .escalate(RepositoryApprovalStoreError::Io)
                .message("read repository approval record")
        })?;
        let approval: RepositoryApproval = serde_json::from_slice(&bytes).map_err(|source| {
            source
                .escalate(RepositoryApprovalStoreError::InvalidRecord)
                .message("decode repository approval record")
        })?;
        approval.validate()?;
        if self.record_path(&approval.id) != path {
            return Err(RepositoryApprovalStoreError::InvalidRecord.report());
        }
        Ok(approval)
    }

    fn record_path(&self, id: &RepositoryApprovalId) -> PathBuf {
        self.directory.join(format!("{}.json", id.0))
    }

    fn claim_path(&self, id: &RepositoryApprovalId) -> PathBuf {
        self.directory.join(format!("{}.claim", id.0))
    }
}

/// Caller-relevant durable approval store failures.
#[derive(Debug, Error)]
pub(crate) enum RepositoryApprovalStoreError {
    /// Approval record is absent.
    #[error("repository approval request does not exist")]
    NotFound,
    /// Approval identifier is already pending.
    #[error("repository approval request already exists")]
    AlreadyExists,
    /// Approval is already claimed for background publication.
    #[error("repository approval publication is already running")]
    AlreadyPublishing,
    /// Approval is not claimed for background publication.
    #[error("repository approval publication is not running")]
    NotPublishing,
    /// Approval has left its initial pending state.
    #[error("repository approval is no longer awaiting its initial decision")]
    NotPending,
    /// Stored record is malformed.
    #[error("repository approval record is invalid")]
    InvalidRecord,
    /// Stored record uses an unsupported format version.
    #[error("repository approval record version {0} is unsupported")]
    UnsupportedVersion(u32),
    /// Approval directory or record is not a safe real path.
    #[error("repository approval path is unsafe: {0}")]
    UnsafePath(PathBuf),
    /// Workspace contains too many pending approvals.
    #[error("repository approval record limit was exceeded")]
    RecordLimit,
    /// Approval record exceeds its configured bound.
    #[error("repository approval record exceeds its size limit")]
    RecordTooLarge,
    /// Filesystem operation failed.
    #[error("repository approval storage failed")]
    Io,
}

fn sync_directory(path: &Path) -> Result<(), Report<RepositoryApprovalStoreError>> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| {
            source
                .escalate(RepositoryApprovalStoreError::Io)
                .message("sync repository approval directory")
        })
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use super::*;

    /// Verifies background claims survive reopening, expose failures, and
    /// disappear only after durable completion.
    #[test]
    fn approval_claims_round_trip_durably() {
        let temporary = tempfile::tempdir().expect("create temporary directory");
        let store = RepositoryApprovalStore::open(temporary.path()).expect("open approval store");
        let approval = RepositoryApproval::new(
            RepositoryApprovalId::generate(),
            PodId::generate().0.to_string(),
            "src/tascarrel".to_owned(),
            "0".repeat(64),
            "https://example.invalid/tascarrel.git".to_owned(),
            vec![RepositoryApprovalUpdate {
                source: "refs/tascarrel/capture".to_owned(),
                destination: "refs/heads/main".to_owned(),
                expected: None,
                proposed: "0123456789012345678901234567890123456789".to_owned(),
                allow_rewrite: false,
            }],
            None,
            None,
        );
        store.create(&approval).expect("create approval");

        let reopened =
            RepositoryApprovalStore::open(temporary.path()).expect("reopen approval store");
        let listed = reopened.list().expect("list approvals");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, approval.id);
        reopened
            .postpone(&approval.id)
            .expect("postpone approval overlay");
        assert!(
            reopened
                .read(&approval.id)
                .expect("read postponed approval")
                .postponed
        );
        reopened
            .postpone(&approval.id)
            .expect("repeat approval postponement");
        let claimed = reopened
            .claim(&approval.id)
            .expect("claim approval")
            .expect("approval was not already claimed");
        assert!(claimed.publishing);
        assert!(
            reopened
                .claim(&approval.id)
                .expect("repeat approval claim")
                .is_none()
        );
        assert!(reopened.list().expect("list claimed approval")[0].publishing);
        reopened
            .fail_claim(&approval.id, "upstream unavailable".to_owned())
            .expect("release failed claim");
        let failed = &reopened.list().expect("list failed approval")[0];
        assert!(!failed.publishing);
        assert_eq!(failed.last_error.as_deref(), Some("upstream unavailable"));
        reopened
            .claim(&approval.id)
            .expect("reclaim approval")
            .expect("approval was not already claimed");
        reopened
            .complete_claim(&approval.id)
            .expect("complete approval");
        assert!(reopened.list().expect("list resolved approvals").is_empty());
    }

    /// Verifies approval loading rejects records redirected outside the
    /// workspace cache.
    #[test]
    fn approval_records_reject_symlinks() {
        let temporary = tempfile::tempdir().expect("create temporary directory");
        let store = RepositoryApprovalStore::open(temporary.path()).expect("open approval store");
        let id = RepositoryApprovalId::generate();
        let outside = temporary.path().join("outside.json");
        fs::write(&outside, b"{}").expect("write outside record");
        symlink(&outside, store.record_path(&id)).expect("create approval symlink");

        let report = store.read(&id).expect_err("symlinked approval must fail");
        assert!(matches!(
            report.error(),
            RepositoryApprovalStoreError::UnsafePath(_)
        ));
    }
}
