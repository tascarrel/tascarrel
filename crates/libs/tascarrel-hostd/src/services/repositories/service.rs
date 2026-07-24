//! Repository inventory, publication approval, and resumable change
//! observation.

use std::fs;
use std::path::PathBuf;
use std::str::FromStr as _;
use std::sync::Arc;
use std::time::Duration;

use reportify::ErrorExt as _;
use reportify::Report;
use serde::Serialize;
use sha2::Digest as _;
use sha2::Sha256;
use tascarrel_api::types::pods::PodId as ApiPodId;
use tascarrel_api::types::repositories as api;
use tascarrel_api::types::workspaces::WorkspaceName as ApiWorkspaceName;
use tascarrel_git::RepositoryStatistics;
use tascarrel_protocol::WorkspaceName;
use thiserror::Error;
use tokio::time::Interval;
use tokio::time::MissedTickBehavior;
use tokio::time::interval;
use tracing::warn;

use super::HostRepositoryCache;
use super::HostRepositoryCacheReady;
use super::HostRepositoryManager;
use super::HostRepositoryStatus;
use super::HostRepositoryVersion;
use super::RepositoryApproval;
use super::RepositoryPushState;
use super::RepositoryPushStatusStore;

/// Host-owned repository inventory and publication approval service.
#[derive(Clone, Debug)]
pub struct RepositoryService {
    config: Arc<RepositoryServiceConfig>,
}

impl RepositoryService {
    /// Creates a repository inventory service.
    ///
    /// # Errors
    ///
    /// Returns an error for relative paths or zero polling or refresh
    /// intervals.
    pub fn new(config: RepositoryServiceConfig) -> Result<Self, Report<RepositoryServiceError>> {
        config.validate()?;
        Ok(Self {
            config: Arc::new(config),
        })
    }

    /// Periodically refreshes every configured workspace cache until the task
    /// is cancelled.
    pub async fn run_background_refreshes(&self) {
        if let Err(error) = self.resume_background_publications().await {
            warn!(%error, "could not resume repository approval publications");
        }
        let mut ticker = interval(self.config.refresh_interval);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            if let Err(error) = self.refresh_all_workspaces().await {
                warn!(%error, "repository background refresh pass failed");
            }
        }
    }

    /// Opens a resumable inventory subscription for one workspace.
    ///
    /// # Errors
    ///
    /// Returns an error when the workspace name is invalid.
    #[tracing::instrument(level = "debug", skip(self, input), fields(workspace = %input.workspace))]
    pub fn subscribe(
        &self,
        input: api::RepositoryListChangedSubscription,
    ) -> Result<RepositorySubscription, Report<RepositoryServiceError>> {
        let workspace = parse_workspace(&input.workspace)?;
        let mut ticker = interval(self.config.poll_interval);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        Ok(RepositorySubscription {
            service: self.clone(),
            workspace,
            cursor: input.cursor,
            ticker,
        })
    }

    /// Opens a resumable pending-approval subscription for one workspace.
    ///
    /// # Errors
    ///
    /// Returns an error when the workspace name is invalid.
    #[tracing::instrument(level = "debug", skip(self, input), fields(workspace = %input.workspace))]
    pub fn subscribe_approvals(
        &self,
        input: api::RepositoryApprovalRequestListChangedSubscription,
    ) -> Result<RepositoryApprovalSubscription, Report<RepositoryServiceError>> {
        let workspace = parse_workspace(&input.workspace)?;
        let mut ticker = interval(self.config.poll_interval);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        Ok(RepositoryApprovalSubscription {
            service: self.clone(),
            workspace,
            pod_id: input.pod_id,
            cursor: input.cursor,
            ticker,
        })
    }

    /// Opens a resumable status subscription for one pod push.
    ///
    /// # Errors
    ///
    /// Returns an error when the workspace name is invalid.
    #[tracing::instrument(level = "debug", skip(self, input), fields(workspace = %input.workspace, push_id = %input.push_id.0))]
    pub fn subscribe_push_status(
        &self,
        input: api::RepositoryPushStatusChangedSubscription,
    ) -> Result<RepositoryPushStatusSubscription, Report<RepositoryServiceError>> {
        let workspace = parse_workspace(&input.workspace)?;
        let mut ticker = interval(self.config.poll_interval);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        Ok(RepositoryPushStatusSubscription {
            service: self.clone(),
            workspace,
            pod_id: input.pod_id,
            push_id: input.push_id,
            cursor: input.cursor,
            ticker,
        })
    }

    /// Prepares configured caches without refreshing already-usable caches.
    ///
    /// # Errors
    ///
    /// Returns an error when the workspace is invalid or a missing cache
    /// cannot complete its initial upstream refresh.
    #[tracing::instrument(
        name = "tascarrel_host.repositories.prepare_snapshot",
        level = "info",
        skip(self),
        fields(workspace = %input.workspace),
        err
    )]
    pub async fn prepare_snapshot(
        &self,
        input: api::PrepareRepositorySnapshotAction,
    ) -> Result<api::PrepareRepositorySnapshotOutput, Report<RepositoryServiceError>> {
        let workspace = parse_workspace(&input.workspace)?;
        let manager = self.manager(&workspace)?;
        let repositories = manager.prepare_versions().await.map_err(|error| {
            RepositoryServiceError::Unavailable(bounded_error(&error))
                .report()
                .message("prepare repository snapshot")
        })?;
        Ok(api::PrepareRepositorySnapshotOutput {
            repositories: api_versions(repositories).into(),
        })
    }

    /// Refreshes one or every configured cache and returns its current local
    /// versions.
    ///
    /// # Errors
    ///
    /// Returns an error when the workspace is invalid or an upstream cannot be
    /// refreshed with the host Git environment.
    #[tracing::instrument(
        name = "tascarrel_host.repositories.refresh_snapshot",
        level = "info",
        skip(self),
        fields(workspace = %input.workspace),
        err
    )]
    pub async fn refresh_snapshot(
        &self,
        input: api::RefreshRepositorySnapshotAction,
    ) -> Result<api::RefreshRepositorySnapshotOutput, Report<RepositoryServiceError>> {
        let workspace = parse_workspace(&input.workspace)?;
        let manager = self.manager(&workspace)?;
        if input
            .path
            .as_deref()
            .is_some_and(|path| manager.repository(path).is_none())
        {
            return Err(RepositoryServiceError::InvalidRequest(
                "repository path is not configured".to_owned(),
            )
            .report());
        }
        let repositories = manager
            .refresh_versions(input.path.as_deref())
            .await
            .map_err(|error| {
                RepositoryServiceError::Unavailable(bounded_error(&error))
                    .report()
                    .message("refresh repository snapshot")
            })?;
        Ok(api::RefreshRepositorySnapshotOutput {
            repositories: api_versions(repositories).into(),
        })
    }

    /// Claims, rejects, or postpones one repository approval.
    ///
    /// # Errors
    ///
    /// Returns an error when the workspace or request is invalid, the requested
    /// transition is no longer available, or rejection cleanup fails.
    #[tracing::instrument(
        name = "tascarrel_host.repositories.resolve_approval",
        level = "info",
        skip(self),
        fields(workspace = %input.workspace, approval_id = %input.approval_id.0),
        err
    )]
    pub async fn resolve_approval(
        &self,
        input: api::ResolveRepositoryApprovalAction,
    ) -> Result<api::ResolveRepositoryApprovalOutput, Report<RepositoryServiceError>> {
        let workspace = parse_workspace(&input.workspace)?;
        let manager = self.manager(&workspace)?;
        match input.decision {
            api::RepositoryApprovalDecision::Approve => {
                let approval = manager
                    .claim_approval(&input.approval_id)
                    .map_err(|error| {
                        RepositoryServiceError::Unavailable(bounded_error(&error))
                            .report()
                            .message("claim repository approval")
                    })?;
                if let Some(approval) = approval {
                    Self::spawn_approval_publication(manager, approval.id);
                }
            }
            api::RepositoryApprovalDecision::Reject => {
                manager
                    .reject_approval(&input.approval_id)
                    .await
                    .map_err(|error| {
                        RepositoryServiceError::Unavailable(bounded_error(&error))
                            .report()
                            .message("reject repository approval")
                    })?;
            }
            api::RepositoryApprovalDecision::Postpone => {
                manager
                    .postpone_approval(&input.approval_id)
                    .map_err(|error| {
                        RepositoryServiceError::Unavailable(bounded_error(&error))
                            .report()
                            .message("postpone repository approval")
                    })?;
            }
        }
        Ok(api::ResolveRepositoryApprovalOutput {})
    }

    #[tracing::instrument(
        name = "tascarrel_host.repositories.snapshot",
        level = "debug",
        skip(self),
        fields(workspace = %workspace),
        err
    )]
    async fn snapshot(
        &self,
        workspace: &WorkspaceName,
    ) -> Result<api::RepositoryListChangedEvent, Report<RepositoryServiceError>> {
        let manager = self.manager(workspace)?;
        let repositories = manager
            .inventory()
            .await
            .into_iter()
            .map(api_repository)
            .collect::<Result<Vec<_>, _>>()?;
        let value = api::RepositoryList {
            repositories: repositories.into(),
        };
        let revision = snapshot_revision(&value)?;
        Ok(api::RepositoryListChangedEvent { revision, value })
    }

    #[tracing::instrument(
        name = "tascarrel_host.repositories.approval_snapshot",
        level = "debug",
        skip(self),
        fields(workspace = %workspace),
        err
    )]
    async fn approval_snapshot(
        &self,
        workspace: &WorkspaceName,
    ) -> Result<api::RepositoryApprovalRequestListChangedEvent, Report<RepositoryServiceError>>
    {
        let manager = self.manager(workspace)?;
        let requests = manager
            .approvals()
            .map_err(|error| {
                RepositoryServiceError::Internal(bounded_error(&error))
                    .report()
                    .message("load repository approvals")
            })?
            .into_iter()
            .map(api_approval)
            .collect::<Result<Vec<_>, _>>()?;
        let value = api::RepositoryApprovalRequestList {
            requests: requests.into(),
        };
        let revision = snapshot_revision(&value)?;
        Ok(api::RepositoryApprovalRequestListChangedEvent { revision, value })
    }

    #[tracing::instrument(
        name = "tascarrel_host.repositories.push_status_snapshot",
        level = "debug",
        skip(self),
        fields(workspace = %workspace, push_id = %push_id.0),
        err
    )]
    fn push_status_snapshot(
        &self,
        workspace: &WorkspaceName,
        pod_id: &ApiPodId,
        push_id: &api::RepositoryPushId,
    ) -> Result<api::RepositoryPushStatusChangedEvent, Report<RepositoryServiceError>> {
        self.workspace_directory(workspace)?;
        let store = RepositoryPushStatusStore::open(
            &self
                .config
                .cache_directory
                .join("workspaces")
                .join(workspace.as_str()),
        )
        .map_err(|error| {
            RepositoryServiceError::Internal(bounded_error(&error))
                .report()
                .message("open repository push status store")
        })?;
        let status = store.read(push_id, pod_id.0.as_ref()).map_err(|error| {
            RepositoryServiceError::InvalidRequest(bounded_error(&error))
                .report()
                .message("load repository push status")
        })?;
        let value = api_push_status(status.state);
        let revision = snapshot_revision(&value)?;
        Ok(api::RepositoryPushStatusChangedEvent { revision, value })
    }

    fn spawn_approval_publication(
        manager: Arc<HostRepositoryManager>,
        approval_id: api::RepositoryApprovalId,
    ) {
        std::mem::drop(tokio::spawn(async move {
            if let Err(error) = manager.publish_claimed_approval(&approval_id).await {
                let diagnostic = bounded_error(&error);
                warn!(approval_id = %approval_id.0, %error, "background repository publication failed");
                if let Err(release) = manager.fail_approval_publication(&approval_id, diagnostic) {
                    warn!(approval_id = %approval_id.0, error = %release, "could not release failed repository publication");
                }
            }
        }));
    }

    async fn resume_background_publications(&self) -> Result<(), Report<RepositoryServiceError>> {
        let mut directories = tokio::fs::read_dir(&self.config.workspaces_directory)
            .await
            .map_err(|error| {
                error
                    .escalate(RepositoryServiceError::Internal(
                        "could not enumerate workspaces for approval recovery".to_owned(),
                    ))
                    .message("list repository workspaces")
            })?;
        while let Some(entry) = directories.next_entry().await.map_err(|error| {
            error
                .escalate(RepositoryServiceError::Internal(
                    "could not enumerate workspaces for approval recovery".to_owned(),
                ))
                .message("read repository workspace entry")
        })? {
            let Some(name) = entry.file_name().to_str().map(ToOwned::to_owned) else {
                continue;
            };
            let Ok(workspace) = WorkspaceName::new(name) else {
                continue;
            };
            let metadata = match entry.file_type().await {
                Ok(metadata) => metadata,
                Err(error) => {
                    warn!(workspace = %workspace, %error, "could not inspect approval workspace entry");
                    continue;
                }
            };
            if !metadata.is_dir() || metadata.is_symlink() {
                continue;
            }
            let manager = match self.manager(&workspace) {
                Ok(manager) => manager,
                Err(error) => {
                    warn!(workspace = %workspace, %error, "could not load repository manager for approval recovery");
                    continue;
                }
            };
            let approvals = match manager.claimed_approvals() {
                Ok(approvals) => approvals,
                Err(error) => {
                    warn!(workspace = %workspace, %error, "could not load claimed repository approvals");
                    continue;
                }
            };
            for approval in approvals {
                Self::spawn_approval_publication(Arc::clone(&manager), approval.id);
            }
        }
        Ok(())
    }

    fn manager(
        &self,
        workspace: &WorkspaceName,
    ) -> Result<Arc<HostRepositoryManager>, Report<RepositoryServiceError>> {
        let workspace_directory = self.workspace_directory(workspace)?;
        HostRepositoryManager::load(
            self.config.git.clone(),
            self.config
                .cache_directory
                .join("workspaces")
                .join(workspace.as_str()),
            &workspace_directory.join("config.toml"),
        )
        .map_err(|error| {
            RepositoryServiceError::Internal(bounded_error(&error))
                .report()
                .message("load repository manager")
        })
    }

    fn workspace_directory(
        &self,
        workspace: &WorkspaceName,
    ) -> Result<PathBuf, Report<RepositoryServiceError>> {
        let workspace_directory = self.config.workspaces_directory.join(workspace.as_str());
        let metadata = fs::symlink_metadata(&workspace_directory).map_err(|error| {
            error
                .escalate(RepositoryServiceError::Unavailable(
                    "workspace configuration is unavailable".to_owned(),
                ))
                .message("inspect repository workspace directory")
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(RepositoryServiceError::Unavailable(
                "workspace configuration is not a safe directory".to_owned(),
            )
            .report());
        }
        Ok(workspace_directory)
    }

    async fn refresh_all_workspaces(&self) -> Result<(), Report<RepositoryServiceError>> {
        let mut directories = tokio::fs::read_dir(&self.config.workspaces_directory)
            .await
            .map_err(|error| {
                error
                    .escalate(RepositoryServiceError::Internal(
                        "could not enumerate workspaces for repository refresh".to_owned(),
                    ))
                    .message("list repository workspaces")
            })?;
        while let Some(entry) = directories.next_entry().await.map_err(|error| {
            error
                .escalate(RepositoryServiceError::Internal(
                    "could not enumerate workspaces for repository refresh".to_owned(),
                ))
                .message("read repository workspace entry")
        })? {
            let Some(name) = entry.file_name().to_str().map(ToOwned::to_owned) else {
                continue;
            };
            let Ok(workspace) = WorkspaceName::new(name) else {
                continue;
            };
            let metadata = match entry.file_type().await {
                Ok(metadata) => metadata,
                Err(error) => {
                    warn!(workspace = %workspace, %error, "could not inspect repository workspace entry");
                    continue;
                }
            };
            if !metadata.is_dir() || metadata.is_symlink() {
                continue;
            }
            let manager = match self.manager(&workspace) {
                Ok(manager) => manager,
                Err(error) => {
                    warn!(workspace = %workspace, %error, "could not load repository manager for background refresh");
                    continue;
                }
            };
            if let Err(error) = manager.refresh_versions(None).await {
                warn!(workspace = %workspace, %error, "repository background refresh failed");
            }
        }
        Ok(())
    }
}

/// Filesystem and Git settings used to inspect repository caches.
#[derive(Clone, Debug)]
pub struct RepositoryServiceConfig {
    /// Host Git executable, preserving the host user's Git environment.
    pub git: PathBuf,
    /// Directory containing one configuration directory per workspace.
    pub workspaces_directory: PathBuf,
    /// Private root containing workspace-scoped bare object stores.
    pub cache_directory: PathBuf,
    /// Delay between cache inventory observations.
    pub poll_interval: Duration,
    /// Delay between automatic upstream refresh passes.
    pub refresh_interval: Duration,
}

impl RepositoryServiceConfig {
    /// Creates repository inventory configuration with default observation
    /// and upstream refresh intervals.
    #[must_use]
    pub fn new(
        git: impl Into<PathBuf>,
        workspaces_directory: impl Into<PathBuf>,
        cache_directory: impl Into<PathBuf>,
    ) -> Self {
        Self {
            git: git.into(),
            workspaces_directory: workspaces_directory.into(),
            cache_directory: cache_directory.into(),
            poll_interval: DEFAULT_POLL_INTERVAL,
            refresh_interval: DEFAULT_REFRESH_INTERVAL,
        }
    }

    fn validate(&self) -> Result<(), Report<RepositoryServiceError>> {
        if !self.git.is_absolute()
            || !self.workspaces_directory.is_absolute()
            || !self.cache_directory.is_absolute()
            || self.poll_interval.is_zero()
            || self.refresh_interval.is_zero()
        {
            return Err(RepositoryServiceError::InvalidConfiguration.report());
        }
        Ok(())
    }
}

const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(2);
const DEFAULT_REFRESH_INTERVAL: Duration = Duration::from_mins(5);

/// Resumable stream of complete repository inventory changes.
pub struct RepositorySubscription {
    service: RepositoryService,
    workspace: WorkspaceName,
    cursor: Option<api::RepositoryRevision>,
    ticker: Interval,
}

impl RepositorySubscription {
    /// Waits for and returns the next inventory whose revision differs from the
    /// cursor.
    ///
    /// # Errors
    ///
    /// Returns an error when configuration or cache state cannot be inspected.
    pub async fn recv(
        &mut self,
    ) -> Result<api::RepositoryListChangedEvent, Report<RepositoryServiceError>> {
        loop {
            self.ticker.tick().await;
            let event = self.service.snapshot(&self.workspace).await?;
            if self.cursor.as_ref() == Some(&event.revision) {
                continue;
            }
            self.cursor = Some(event.revision.clone());
            return Ok(event);
        }
    }
}

/// Resumable stream of complete pending repository approval lists.
pub struct RepositoryApprovalSubscription {
    service: RepositoryService,
    workspace: WorkspaceName,
    pod_id: Option<ApiPodId>,
    cursor: Option<api::RepositoryRevision>,
    ticker: Interval,
}

impl RepositoryApprovalSubscription {
    /// Waits for and returns the next pending approval list whose revision
    /// differs from the cursor.
    ///
    /// # Errors
    ///
    /// Returns an error when workspace configuration or durable approval state
    /// cannot be inspected.
    pub async fn recv(
        &mut self,
    ) -> Result<api::RepositoryApprovalRequestListChangedEvent, Report<RepositoryServiceError>>
    {
        loop {
            self.ticker.tick().await;
            let mut event = self.service.approval_snapshot(&self.workspace).await?;
            if let Some(pod_id) = &self.pod_id {
                event.value.requests = event
                    .value
                    .requests
                    .iter()
                    .filter(|request| request.pod_id == *pod_id)
                    .cloned()
                    .collect::<Vec<_>>()
                    .into();
                event.revision = snapshot_revision(&event.value)?;
            }
            if self.cursor.as_ref() == Some(&event.revision) {
                continue;
            }
            self.cursor = Some(event.revision.clone());
            return Ok(event);
        }
    }
}

/// Resumable stream of one pod push's durable status changes.
pub struct RepositoryPushStatusSubscription {
    service: RepositoryService,
    workspace: WorkspaceName,
    pod_id: ApiPodId,
    push_id: api::RepositoryPushId,
    cursor: Option<api::RepositoryRevision>,
    ticker: Interval,
}

impl RepositoryPushStatusSubscription {
    /// Waits for and returns the next push status whose revision differs from
    /// the cursor.
    ///
    /// # Errors
    ///
    /// Returns an error when the workspace or durable push status cannot be
    /// inspected.
    pub async fn recv(
        &mut self,
    ) -> Result<api::RepositoryPushStatusChangedEvent, Report<RepositoryServiceError>> {
        loop {
            self.ticker.tick().await;
            let event =
                self.service
                    .push_status_snapshot(&self.workspace, &self.pod_id, &self.push_id)?;
            if self.cursor.as_ref() == Some(&event.revision) {
                continue;
            }
            self.cursor = Some(event.revision.clone());
            return Ok(event);
        }
    }
}

/// Caller-relevant repository service failure categories.
#[derive(Debug, Error)]
pub enum RepositoryServiceError {
    /// The service was constructed with invalid paths or polling policy.
    #[error("repository service configuration is invalid")]
    InvalidConfiguration,
    /// A subscription input is invalid.
    #[error("invalid repository request: {0}")]
    InvalidRequest(String),
    /// The requested workspace configuration is unavailable.
    #[error("repository inventory is unavailable: {0}")]
    Unavailable(String),
    /// Repository configuration or cache inspection failed unexpectedly.
    #[error("repository service failed: {0}")]
    Internal(String),
}

fn parse_workspace(
    workspace: &ApiWorkspaceName,
) -> Result<WorkspaceName, Report<RepositoryServiceError>> {
    WorkspaceName::new(workspace.as_str()).map_err(|error| {
        error
            .escalate(RepositoryServiceError::InvalidRequest(
                "workspace name is invalid".to_owned(),
            ))
            .message("validate repository workspace")
    })
}

fn snapshot_revision(
    value: &impl Serialize,
) -> Result<api::RepositoryRevision, Report<RepositoryServiceError>> {
    let encoded = serde_json::to_vec(value).map_err(|error| {
        error
            .escalate(RepositoryServiceError::Internal(
                "could not derive repository state revision".to_owned(),
            ))
            .message("encode repository state revision")
    })?;
    Ok(api::RepositoryRevision::new(format!(
        "{:x}",
        Sha256::digest(encoded)
    )))
}

fn api_approval(
    approval: RepositoryApproval,
) -> Result<api::RepositoryApprovalRequest, Report<RepositoryServiceError>> {
    let pod_id = ApiPodId::from_str(&approval.pod_id).map_err(|error| {
        error
            .escalate(RepositoryServiceError::Internal(
                "approval contains an invalid pod identifier".to_owned(),
            ))
            .message("decode repository approval pod")
    })?;
    let updates = approval
        .updates
        .into_iter()
        .map(|update| api::RepositoryApprovalUpdate {
            reference: update.destination.into(),
            previous_object: update.expected.map(Into::into),
            proposed_object: update.proposed.into(),
            rewrites: update.allow_rewrite,
        })
        .collect::<Vec<_>>();
    let status = if approval.publishing {
        api::RepositoryApprovalStatus::Publishing
    } else if let Some(error) = approval.last_error {
        api::RepositoryApprovalStatus::Failed(error.into())
    } else {
        api::RepositoryApprovalStatus::Pending
    };
    Ok(api::RepositoryApprovalRequest {
        id: approval.id,
        pod_id,
        path: approval.path.into(),
        source: approval.source.into(),
        created_at: approval.created_at,
        status,
        postponed: approval.postponed,
        updates: updates.into(),
    })
}

fn api_push_status(state: RepositoryPushState) -> api::RepositoryPushStatus {
    match state {
        RepositoryPushState::Published => api::RepositoryPushStatus::Published,
        RepositoryPushState::ApprovalRequired(approval_id) => {
            api::RepositoryPushStatus::ApprovalRequired(approval_id)
        }
        RepositoryPushState::Denied(message) => api::RepositoryPushStatus::Denied(message.into()),
        RepositoryPushState::Rejected => api::RepositoryPushStatus::Rejected,
        RepositoryPushState::Failed(message) => api::RepositoryPushStatus::Failed(message.into()),
    }
}

fn bounded_error(error: &impl std::fmt::Display) -> String {
    const MAX_CHARS: usize = 2048;

    error.to_string().chars().take(MAX_CHARS).collect()
}

fn api_repository(
    repository: HostRepositoryStatus,
) -> Result<api::Repository, Report<RepositoryServiceError>> {
    let cache = match repository.cache {
        HostRepositoryCache::Missing => api::RepositoryCacheState::Missing,
        HostRepositoryCache::Ready(ready) => {
            api::RepositoryCacheState::Ready(api_statistics(&ready)?)
        }
        HostRepositoryCache::Failed(message) => {
            api::RepositoryCacheState::Failed(api::RepositoryCacheFailure {
                message: message.into(),
            })
        }
    };
    Ok(api::Repository {
        path: repository.path.into(),
        source: repository.source.into(),
        cache,
    })
}

fn api_statistics(
    ready: &HostRepositoryCacheReady,
) -> Result<api::RepositoryCacheStatistics, Report<RepositoryServiceError>> {
    let statistics: &RepositoryStatistics = &ready.statistics;
    Ok(api::RepositoryCacheStatistics {
        cache_id: ready.state.id.clone(),
        version: ready.state.version,
        version_updated_at: ready.state.version_updated_at,
        refreshed_at: ready.state.refreshed_at,
        refresh_error: ready.state.refresh_error.clone().map(Into::into),
        branches: usize_to_u64(statistics.branches)?,
        tags: usize_to_u64(statistics.tags)?,
        captures: usize_to_u64(statistics.captures)?,
        loose_objects: statistics.loose_objects,
        packed_objects: statistics.packed_objects,
        packs: statistics.packs,
        size_bytes: statistics.size_bytes,
        garbage_bytes: statistics.garbage_bytes,
    })
}

fn api_versions(versions: Vec<HostRepositoryVersion>) -> Vec<api::RepositoryCacheVersion> {
    versions
        .into_iter()
        .map(|version| api::RepositoryCacheVersion {
            path: version.path.into(),
            cache_id: version.cache_id,
            version: version.version,
            updated_at: version.updated_at,
        })
        .collect()
}

fn usize_to_u64(value: usize) -> Result<u64, Report<RepositoryServiceError>> {
    u64::try_from(value).map_err(|error| {
        error
            .escalate(RepositoryServiceError::Internal(
                "repository statistic exceeds the API range".to_owned(),
            ))
            .message("convert repository statistic")
    })
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tascarrel_api::ids::PodId;
    use tascarrel_api::ids::RepositoryApprovalId;
    use tascarrel_api::ids::RepositoryPushId;
    use tascarrel_api::types::workspaces::WorkspaceName as ApiWorkspaceName;

    use super::*;
    use crate::services::repositories::RepositoryApprovalStore;
    use crate::services::repositories::RepositoryApprovalUpdate;
    use crate::services::repositories::RepositoryPushStatus;
    use crate::services::repositories::RepositoryPushStatusStore;

    /// A subscription suppresses an unchanged resumed inventory and emits a
    /// new complete snapshot after repository configuration changes.
    #[tokio::test]
    async fn subscription_resumes_from_inventory_revision() {
        let temporary = tempfile::tempdir().expect("create temporary directory");
        let workspaces = temporary.path().join("workspaces");
        let workspace = workspaces.join("demo");
        fs::create_dir_all(&workspace).expect("create workspace directory");
        fs::write(
            workspace.join("config.toml"),
            "[repos.source]\nsource = 'https://example.invalid/source.git'\n",
        )
        .expect("write workspace configuration");
        let mut config = RepositoryServiceConfig::new(
            std::env::current_exe().expect("resolve test executable"),
            workspaces,
            temporary.path().join("cache"),
        );
        config.poll_interval = Duration::from_millis(10);
        let service = RepositoryService::new(config).expect("create repository service");
        let input = api::RepositoryListChangedSubscription {
            workspace: ApiWorkspaceName::new("demo"),
            cursor: None,
        };
        let first = service
            .subscribe(input)
            .expect("subscribe to inventory")
            .recv()
            .await
            .expect("receive first inventory");
        assert_eq!(first.value.repositories.len(), 1);

        let resumed = api::RepositoryListChangedSubscription {
            workspace: ApiWorkspaceName::new("demo"),
            cursor: Some(first.revision),
        };
        let mut subscription = service
            .subscribe(resumed)
            .expect("resume inventory subscription");
        assert!(
            tokio::time::timeout(Duration::from_millis(30), subscription.recv())
                .await
                .is_err()
        );
        fs::write(
            workspace.join("config.toml"),
            "[repos.source]\nsource = 'https://example.invalid/source.git'\n\
             [repos.docs]\nsource = 'https://example.invalid/docs.git'\n",
        )
        .expect("update workspace configuration");
        let changed = tokio::time::timeout(Duration::from_secs(1), subscription.recv())
            .await
            .expect("inventory change did not arrive")
            .expect("receive changed inventory");
        assert_eq!(changed.value.repositories.len(), 2);
    }

    /// A resumed approval subscription emits after a request is created and
    /// again after its overlay is durably postponed.
    #[tokio::test]
    async fn repository_approval_subscription_resumes_from_complete_revision() {
        let temporary = tempfile::tempdir().expect("create temporary directory");
        let workspaces = temporary.path().join("workspaces");
        let workspace = workspaces.join("demo");
        fs::create_dir_all(&workspace).expect("create workspace directory");
        fs::write(
            workspace.join("config.toml"),
            "[repos.source]\nsource = 'https://example.invalid/source.git'\n",
        )
        .expect("write workspace configuration");
        let cache = temporary.path().join("cache");
        let mut config = RepositoryServiceConfig::new(
            std::env::current_exe().expect("resolve test executable"),
            &workspaces,
            &cache,
        );
        config.poll_interval = Duration::from_millis(10);
        let service = RepositoryService::new(config).expect("create repository service");
        let input = api::RepositoryApprovalRequestListChangedSubscription {
            workspace: ApiWorkspaceName::new("demo"),
            pod_id: None,
            cursor: None,
        };
        let first = service
            .subscribe_approvals(input)
            .expect("subscribe to approvals")
            .recv()
            .await
            .expect("receive initial approvals");
        assert!(first.value.requests.is_empty());

        let resumed = api::RepositoryApprovalRequestListChangedSubscription {
            workspace: ApiWorkspaceName::new("demo"),
            pod_id: None,
            cursor: Some(first.revision),
        };
        let mut subscription = service
            .subscribe_approvals(resumed)
            .expect("resume approval subscription");
        assert!(
            tokio::time::timeout(Duration::from_millis(30), subscription.recv())
                .await
                .is_err()
        );
        let approvals = RepositoryApprovalStore::open(&cache.join("workspaces").join("demo"))
            .expect("open approval store");
        let approval = RepositoryApproval::new(
            RepositoryApprovalId::generate(),
            PodId::generate().0.to_string(),
            "source".to_owned(),
            "0".repeat(64),
            "https://example.invalid/source.git".to_owned(),
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
        approvals
            .create(&approval)
            .expect("create pending approval");

        let changed = tokio::time::timeout(Duration::from_secs(1), subscription.recv())
            .await
            .expect("approval change did not arrive")
            .expect("receive changed approvals");
        assert_eq!(changed.value.requests.len(), 1);
        assert_eq!(changed.value.requests[0].id, approval.id);
        assert!(!changed.value.requests[0].postponed);

        service
            .resolve_approval(api::ResolveRepositoryApprovalAction {
                workspace: ApiWorkspaceName::new("demo"),
                approval_id: approval.id,
                decision: api::RepositoryApprovalDecision::Postpone,
            })
            .await
            .expect("postpone approval overlay");
        let postponed = tokio::time::timeout(Duration::from_secs(1), subscription.recv())
            .await
            .expect("postponed approval change did not arrive")
            .expect("receive postponed approval");
        assert!(postponed.value.requests[0].postponed);
    }

    /// Independent approval actions return after durable claims while their
    /// background publications continue.
    #[tokio::test]
    async fn repository_approval_actions_return_before_publication() {
        let temporary = tempfile::tempdir().expect("create temporary directory");
        let workspaces = temporary.path().join("workspaces");
        let workspace = workspaces.join("demo");
        fs::create_dir_all(&workspace).expect("create workspace directory");
        let source = "https://example.invalid/source.git";
        fs::write(
            workspace.join("config.toml"),
            format!("[repos.source]\nsource = {source:?}\n"),
        )
        .expect("write workspace configuration");
        let cache = temporary.path().join("cache");
        let service = RepositoryService::new(RepositoryServiceConfig::new(
            std::env::current_exe().expect("resolve test executable"),
            workspaces,
            &cache,
        ))
        .expect("create repository service");
        let approvals = RepositoryApprovalStore::open(&cache.join("workspaces").join("demo"))
            .expect("open approval store");
        let repository_id = format!("{:x}", Sha256::digest(source.as_bytes()));
        let approval_ids = [
            RepositoryApprovalId::generate(),
            RepositoryApprovalId::generate(),
        ];
        for approval_id in &approval_ids {
            approvals
                .create(&RepositoryApproval::new(
                    approval_id.clone(),
                    PodId::generate().0.to_string(),
                    "source".to_owned(),
                    repository_id.clone(),
                    source.to_owned(),
                    vec![RepositoryApprovalUpdate {
                        source: "refs/tascarrel/capture".to_owned(),
                        destination: "refs/heads/main".to_owned(),
                        expected: None,
                        proposed: "0123456789012345678901234567890123456789".to_owned(),
                        allow_rewrite: false,
                    }],
                    None,
                    None,
                ))
                .expect("create pending approval");
        }

        for approval_id in &approval_ids {
            service
                .resolve_approval(api::ResolveRepositoryApprovalAction {
                    workspace: ApiWorkspaceName::new("demo"),
                    approval_id: approval_id.clone(),
                    decision: api::RepositoryApprovalDecision::Approve,
                })
                .await
                .expect("start background publication");
        }

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let listed = approvals.list().expect("list approvals");
                if listed.len() == approval_ids.len()
                    && listed
                        .iter()
                        .all(|approval| !approval.publishing && approval.last_error.is_some())
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("background publication failures did not complete");
    }

    /// Rejection removes a stale approval and wakes its resumed push-status
    /// subscription with a terminal result.
    #[tokio::test]
    async fn rejection_does_not_require_the_original_repository_configuration() {
        let temporary = tempfile::tempdir().expect("create temporary directory");
        let workspaces = temporary.path().join("workspaces");
        let workspace = workspaces.join("demo");
        fs::create_dir_all(&workspace).expect("create workspace directory");
        fs::write(workspace.join("config.toml"), "").expect("write workspace configuration");
        let cache = temporary.path().join("cache");
        let service = RepositoryService::new(RepositoryServiceConfig::new(
            std::env::current_exe().expect("resolve test executable"),
            workspaces,
            &cache,
        ))
        .expect("create repository service");
        let approvals = RepositoryApprovalStore::open(&cache.join("workspaces").join("demo"))
            .expect("open approval store");
        let statuses = RepositoryPushStatusStore::open(&cache.join("workspaces").join("demo"))
            .expect("open push status store");
        let pod_id = PodId::generate();
        let push_id = RepositoryPushId::generate();
        let approval = RepositoryApproval::new(
            RepositoryApprovalId::generate(),
            pod_id.0.to_string(),
            "source".to_owned(),
            "0".repeat(64),
            "https://example.invalid/source.git".to_owned(),
            vec![RepositoryApprovalUpdate {
                source: "refs/tascarrel/capture".to_owned(),
                destination: "refs/heads/main".to_owned(),
                expected: None,
                proposed: "0123456789012345678901234567890123456789".to_owned(),
                allow_rewrite: false,
            }],
            Some(push_id.clone()),
            None,
        );
        statuses
            .create(&RepositoryPushStatus::new(
                push_id.clone(),
                pod_id.0.to_string(),
                RepositoryPushState::ApprovalRequired(approval.id.clone()),
            ))
            .expect("create pending push status");
        approvals
            .create(&approval)
            .expect("create pending approval");
        let initial = service
            .subscribe_push_status(api::RepositoryPushStatusChangedSubscription {
                workspace: ApiWorkspaceName::new("demo"),
                pod_id: pod_id.clone(),
                push_id: push_id.clone(),
                cursor: None,
            })
            .expect("subscribe to push status")
            .recv()
            .await
            .expect("receive pending push status");
        assert!(matches!(
            initial.value,
            api::RepositoryPushStatus::ApprovalRequired(_)
        ));

        service
            .resolve_approval(api::ResolveRepositoryApprovalAction {
                workspace: ApiWorkspaceName::new("demo"),
                approval_id: approval.id.clone(),
                decision: api::RepositoryApprovalDecision::Reject,
            })
            .await
            .expect("reject stale approval");

        assert!(approvals.list().expect("list approvals").is_empty());
        let terminal = service
            .subscribe_push_status(api::RepositoryPushStatusChangedSubscription {
                workspace: ApiWorkspaceName::new("demo"),
                pod_id,
                push_id,
                cursor: Some(initial.revision),
            })
            .expect("resume push status")
            .recv()
            .await
            .expect("receive rejected push status");
        assert!(matches!(
            terminal.value,
            api::RepositoryPushStatus::Rejected
        ));
    }
}
