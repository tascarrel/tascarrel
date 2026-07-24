//! Durable identity and tracked-ref version records for repository caches.
//!
//! [`RepositoryCacheStateStore`] keeps cache identity independent from its
//! source-derived filesystem name and advances a monotonic version only when
//! the tracked upstream snapshot changes.

use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Path;
use std::path::PathBuf;

use jiff::Timestamp;
use reportify::ErrorExt as _;
use reportify::Report;
use serde::Deserialize;
use serde::Serialize;
use tascarrel_api::ids::RepositoryCacheId;
use thiserror::Error;

const CACHE_STATE_VERSION: u32 = 1;
const MAX_CACHE_STATE_BYTES: u64 = 64 * 1024;

/// Durable state associated with one workspace-isolated repository cache.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RepositoryCacheState {
    schema_version: u32,
    /// Stable identity which changes when the cache state is recreated.
    pub(crate) id: RepositoryCacheId,
    /// Monotonic version of the tracked upstream snapshot.
    pub(crate) version: u64,
    /// Successful refresh sequence used to coalesce concurrent refreshes.
    pub(crate) refresh_sequence: u64,
    /// Digest of tracked branches, tags, and symbolic `HEAD`.
    pub(crate) tracked_refs_digest: Option<String>,
    /// Time at which tracked refs changed and created the current version.
    pub(crate) version_updated_at: Option<Timestamp>,
    /// Last successful upstream refresh time.
    pub(crate) refreshed_at: Option<Timestamp>,
    /// Most recent bounded refresh failure after the last attempt.
    pub(crate) refresh_error: Option<String>,
}

impl RepositoryCacheState {
    /// Creates state for a cache which has not completed its first refresh.
    pub(crate) fn new() -> Self {
        Self {
            schema_version: CACHE_STATE_VERSION,
            id: RepositoryCacheId::generate(),
            version: 0,
            refresh_sequence: 0,
            tracked_refs_digest: None,
            version_updated_at: None,
            refreshed_at: None,
            refresh_error: None,
        }
    }

    /// Records one successful refresh and advances the content version only
    /// when its tracked snapshot changed.
    pub(crate) fn refreshed(
        &mut self,
        tracked_refs_digest: String,
    ) -> Result<bool, Report<RepositoryCacheStateError>> {
        let changed = self.tracked_refs_digest.as_ref() != Some(&tracked_refs_digest);
        let refreshed_at = Timestamp::now();
        if changed {
            self.version = self
                .version
                .checked_add(1)
                .ok_or_else(|| RepositoryCacheStateError::CounterOverflow.report())?;
            self.tracked_refs_digest = Some(tracked_refs_digest);
            self.version_updated_at = Some(refreshed_at);
        }
        self.refresh_sequence = self
            .refresh_sequence
            .checked_add(1)
            .ok_or_else(|| RepositoryCacheStateError::CounterOverflow.report())?;
        self.refreshed_at = Some(refreshed_at);
        self.refresh_error = None;
        Ok(changed)
    }

    /// Records a bounded failure without changing the last successful cache
    /// identity, version, or snapshot digest.
    pub(crate) fn failed(&mut self, message: String) {
        self.refresh_error = Some(message);
    }

    fn validate(&self) -> Result<(), Report<RepositoryCacheStateError>> {
        if self.schema_version != CACHE_STATE_VERSION
            || self
                .tracked_refs_digest
                .as_deref()
                .is_some_and(|digest| !is_digest(digest))
            || (self.version == 0) != self.tracked_refs_digest.is_none()
            || (self.version == 0) != self.version_updated_at.is_none()
        {
            return Err(RepositoryCacheStateError::InvalidRecord.report());
        }
        Ok(())
    }
}

/// Persistence for one source-derived cache state record.
#[derive(Clone, Debug)]
pub(crate) struct RepositoryCacheStateStore {
    root: PathBuf,
    path: PathBuf,
}

impl RepositoryCacheStateStore {
    /// Selects the state record associated with one validated source digest.
    pub(crate) fn new(root: &Path, source_id: &str) -> Self {
        Self {
            root: root.to_owned(),
            path: root.join(format!("{source_id}.cache.json")),
        }
    }

    /// Loads existing state or durably creates a fresh cache identity.
    pub(crate) fn load_or_create(
        &self,
    ) -> Result<RepositoryCacheState, Report<RepositoryCacheStateError>> {
        match fs::symlink_metadata(&self.path) {
            Ok(_) => self.read(),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                let state = RepositoryCacheState::new();
                self.write(&state)?;
                Ok(state)
            }
            Err(source) => Err(source
                .escalate(RepositoryCacheStateError::Io)
                .message("inspect repository cache state")),
        }
    }

    /// Loads an existing cache state record.
    pub(crate) fn read(&self) -> Result<RepositoryCacheState, Report<RepositoryCacheStateError>> {
        let metadata = fs::symlink_metadata(&self.path).map_err(|source| {
            source
                .escalate(RepositoryCacheStateError::Io)
                .message("inspect repository cache state")
        })?;
        if metadata.file_type().is_symlink()
            || !metadata.is_file()
            || metadata.len() > MAX_CACHE_STATE_BYTES
        {
            return Err(RepositoryCacheStateError::UnsafePath(self.path.clone()).report());
        }
        let bytes = fs::read(&self.path).map_err(|source| {
            source
                .escalate(RepositoryCacheStateError::Io)
                .message("read repository cache state")
        })?;
        let state: RepositoryCacheState = serde_json::from_slice(&bytes).map_err(|source| {
            source
                .escalate(RepositoryCacheStateError::InvalidRecord)
                .message("decode repository cache state")
        })?;
        state.validate()?;
        Ok(state)
    }

    /// Atomically publishes current cache state.
    pub(crate) fn write(
        &self,
        state: &RepositoryCacheState,
    ) -> Result<(), Report<RepositoryCacheStateError>> {
        state.validate()?;
        let bytes = serde_json::to_vec(state).map_err(|source| {
            source
                .escalate(RepositoryCacheStateError::InvalidRecord)
                .message("encode repository cache state")
        })?;
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_CACHE_STATE_BYTES {
            return Err(RepositoryCacheStateError::RecordTooLarge.report());
        }
        let temporary = self
            .root
            .join(format!(".cache-state-{}.tmp", uuid::Uuid::new_v4()));
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&temporary)
            .map_err(|source| {
                source
                    .escalate(RepositoryCacheStateError::Io)
                    .message("create temporary repository cache state")
            })?;
        let result = (|| {
            file.write_all(&bytes).map_err(|source| {
                source
                    .escalate(RepositoryCacheStateError::Io)
                    .message("write repository cache state")
            })?;
            file.sync_all().map_err(|source| {
                source
                    .escalate(RepositoryCacheStateError::Io)
                    .message("sync repository cache state")
            })?;
            fs::rename(&temporary, &self.path).map_err(|source| {
                source
                    .escalate(RepositoryCacheStateError::Io)
                    .message("publish repository cache state")
            })?;
            sync_directory(&self.root)
        })();
        if result.is_err()
            && let Err(source) = fs::remove_file(&temporary)
            && source.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(path = %temporary.display(), %source, "could not remove temporary repository cache state");
        }
        result
    }
}

/// Failures while persisting cache identity and tracked-ref versions.
#[derive(Debug, Error)]
pub(crate) enum RepositoryCacheStateError {
    /// Cache state storage could not be accessed durably.
    #[error("repository cache state I/O failed")]
    Io,
    /// Cache state resolved through an unsafe filesystem object.
    #[error("unsafe repository cache state path {0}")]
    UnsafePath(PathBuf),
    /// Cache state could not be decoded or violated its invariants.
    #[error("invalid repository cache state")]
    InvalidRecord,
    /// Encoded cache state exceeded its fixed bound.
    #[error("repository cache state is too large")]
    RecordTooLarge,
    /// A monotonic cache counter exhausted its representation.
    #[error("repository cache state counter overflowed")]
    CounterOverflow,
}

fn is_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn sync_directory(path: &Path) -> Result<(), Report<RepositoryCacheStateError>> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| {
            source
                .escalate(RepositoryCacheStateError::Io)
                .message("sync repository cache state directory")
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A recreated cache gets a distinct identity while unchanged successful
    /// refreshes advance only the coalescing sequence.
    #[test]
    fn cache_identity_and_content_version_have_independent_lifecycles() {
        let temporary = tempfile::tempdir().expect("create temporary directory");
        let store = RepositoryCacheStateStore::new(temporary.path(), &"a".repeat(64));
        let mut state = store.load_or_create().expect("create cache state");
        let first_id = state.id.clone();
        assert!(state.refreshed("b".repeat(64)).expect("record refresh"));
        assert_eq!(state.version, 1);
        let version_updated_at = state.version_updated_at;
        store.write(&state).expect("persist cache state");

        let mut unchanged = store.read().expect("reload cache state");
        assert!(!unchanged.refreshed("b".repeat(64)).expect("record refresh"));
        assert_eq!(unchanged.version, 1);
        assert_eq!(unchanged.version_updated_at, version_updated_at);
        assert_eq!(unchanged.refresh_sequence, 2);

        fs::remove_file(&store.path).expect("remove cache state");
        let recreated = store.load_or_create().expect("recreate cache state");
        assert_ne!(recreated.id, first_id);
        assert_eq!(recreated.version, 0);
    }
}
