//! Host-side repository management, workspace caches, and Git transport
//! adapters.
//!
//! [`HostRepositoryManager`] binds configured repository sources to one
//! workspace-owned object-store root. It refreshes those stores with host
//! credentials, captures exact pod refs, publishes approved branches and tags,
//! and exposes bounded cache inventory without contacting upstreams.

use std::collections::BTreeMap;
use std::fs;
use std::fs::OpenOptions;
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::fs::symlink;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use fs2::FileExt;
use futures_util::StreamExt as _;
use jiff::Timestamp;
use reportify::ErrorExt as _;
use reportify::Report;
use sha2::Digest;
use sha2::Sha256;
use tascarrel_api::ids::RepositoryApprovalId;
use tascarrel_api::ids::RepositoryPushId;
use tascarrel_git::CaptureId;
use tascarrel_git::CapturedReference;
use tascarrel_git::GitBinary;
use tascarrel_git::ObjectId;
use tascarrel_git::ObjectKind;
use tascarrel_git::PodId as GitPodId;
use tascarrel_git::ReceiveNamespace;
use tascarrel_git::ReceivedReferenceUpdate;
use tascarrel_git::RefUpdate;
use tascarrel_git::ReferenceName;
use tascarrel_git::Remote;
use tascarrel_git::RepositoryReference;
use tascarrel_git::RepositoryRefresh;
use tascarrel_git::RepositoryStatistics;
use tascarrel_git::RepositoryStore;
use tascarrel_git::SourceReference;
use tascarrel_git::WorkspaceId as GitWorkspaceId;
use tascarrel_mux::Channel;
use tascarrel_protocol::CodecError;
use tascarrel_protocol::ErrorCode;
use tascarrel_protocol::Framed;
use tascarrel_protocol::GitHostRequest;
use tascarrel_protocol::GitOpenResponse;
use tascarrel_protocol::PodId;
use tascarrel_protocol::RemoteError;
use thiserror::Error;
use tokio::io::AsyncWriteExt as _;
use tokio::sync::Mutex;
use tracing::warn;

use super::RepositoryApproval;
use super::RepositoryApprovalStore;
use super::RepositoryApprovalStoreError;
use super::RepositoryApprovalUpdate;
use super::RepositoryCacheState;
use super::RepositoryCacheStateError;
use super::RepositoryCacheStateStore;
use super::RepositoryPolicy;
use super::RepositoryPolicyError;
use super::RepositoryPushPolicy;
use super::RepositoryPushState;
use super::RepositoryPushStatus;
use super::RepositoryPushStatusStore;
use super::RepositoryPushStatusStoreError;
use crate::services::config::DEFAULT_MAX_CONFIG_BYTES;
use crate::services::config::load_config_file;

const DEFAULT_REFRESH_CONCURRENCY: usize = 4;

/// Workspace-owned bare Git repositories operated with host credentials.
pub struct HostRepositoryManager {
    git: GitBinary,
    root: PathBuf,
    config: PathBuf,
    repositories: BTreeMap<String, HostRepository>,
    approvals: RepositoryApprovalStore,
    push_statuses: RepositoryPushStatusStore,
    operation: Mutex<()>,
}

impl std::fmt::Debug for HostRepositoryManager {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HostRepositoryManager")
            .field("git", &self.git)
            .field("root", &self.root)
            .field("config", &self.config)
            .field("repositories", &self.repositories.len())
            .field("approvals", &self.approvals)
            .field("push_statuses", &self.push_statuses)
            .finish_non_exhaustive()
    }
}

impl HostRepositoryManager {
    /// Loads one workspace's repository policy and prepares its private forge.
    ///
    /// # Errors
    ///
    /// Returns an error for unsafe paths, invalid configuration, or state setup
    /// failure.
    #[tracing::instrument(
        name = "tascarrel_host.repositories.load",
        level = "debug",
        skip_all,
        fields(cache = %root.display(), config = %config.display()),
        err
    )]
    pub fn load(git: PathBuf, root: PathBuf, config: &Path) -> HostRepositoryResult<Arc<Self>> {
        if !root.is_absolute() {
            return Err(invalid_configuration(
                "Git executable and repository state root must be absolute",
            ));
        }
        let git = GitBinary::new(git).map_err(git_report)?;
        let parsed = load_workspace_config(config)?;
        create_private_directory(&root)?;
        ensure_remote_helper(&root)?;
        let approvals = RepositoryApprovalStore::open(&root).map_err(approval_report)?;
        let push_statuses = RepositoryPushStatusStore::open(&root).map_err(push_status_report)?;
        Ok(Arc::new(Self {
            git,
            root,
            config: config.to_owned(),
            repositories: parsed.repos,
            approvals,
            push_statuses,
            operation: Mutex::new(()),
        }))
    }

    /// Returns the credential-bearing upstream source for internal Git use.
    #[must_use]
    pub fn repository(&self, path: &str) -> Option<&str> {
        self.repositories.get(path).map(|repo| repo.source.as_str())
    }

    /// Lists pending publication approvals without contacting upstreams.
    ///
    /// # Errors
    ///
    /// Returns an error when a durable approval record cannot be inspected.
    #[tracing::instrument(
        name = "tascarrel_host.repositories.approvals",
        level = "debug",
        skip(self),
        err
    )]
    pub(crate) fn approvals(&self) -> HostRepositoryResult<Vec<RepositoryApproval>> {
        self.approvals.list().map_err(approval_report)
    }

    /// Inspects every configured repository without contacting its upstream.
    #[tracing::instrument(
        name = "tascarrel_host.repositories.inventory",
        level = "debug",
        skip(self)
    )]
    pub(crate) async fn inventory(&self) -> Vec<HostRepositoryStatus> {
        let _operation = self.operation.lock().await;
        let mut inventory = Vec::with_capacity(self.repositories.len());
        for (path, repository) in &self.repositories {
            let cache_path = self.cache_path(&repository.source);
            let cache = if cache_path.exists() {
                match self.lock_store(&repository.source).await {
                    Ok(_cache_lock) => {
                        match RepositoryStore::open_existing(self.git.clone(), cache_path) {
                            Ok(store) => match (
                                store.statistics().await,
                                self.cache_state_store(&repository.source).read(),
                            ) {
                                (Ok(statistics), Ok(state)) if state.version > 0 => {
                                    HostRepositoryCache::Ready(HostRepositoryCacheReady {
                                        statistics,
                                        state,
                                    })
                                }
                                (Ok(_), Ok(state)) => HostRepositoryCache::Failed(
                                    state.refresh_error.unwrap_or_else(|| {
                                        "repository cache has no successful upstream snapshot"
                                            .to_owned()
                                    }),
                                ),
                                (Err(report), _) => {
                                    HostRepositoryCache::Failed(bounded_git_error(&report))
                                }
                                (_, Err(report)) => {
                                    HostRepositoryCache::Failed(bounded_git_error(&report))
                                }
                            },
                            Err(error) => HostRepositoryCache::Failed(bounded_git_error(&error)),
                        }
                    }
                    Err(error) => HostRepositoryCache::Failed(bounded_git_error(&error)),
                }
            } else {
                HostRepositoryCache::Missing
            };
            inventory.push(HostRepositoryStatus {
                path: path.clone(),
                source: display_remote(&repository.source),
                cache,
            });
        }
        inventory
    }

    /// Refreshes every configured mirror from its upstream.
    ///
    /// # Errors
    ///
    /// Returns an error when a mirror cannot be cloned or fetched.
    #[tracing::instrument(
        name = "tascarrel_host.repositories.sync",
        level = "info",
        skip(self),
        err
    )]
    pub async fn sync(&self) -> HostRepositoryResult<()> {
        self.refresh_versions(None).await.map(|_| ())
    }

    /// Prepares every configured cache and returns its current local versions.
    ///
    /// Existing caches never contact their upstream. A cache which has not
    /// completed its first refresh is initialized from upstream so it can be
    /// used to materialize a seed.
    ///
    /// # Errors
    ///
    /// Returns an error when a cache cannot be initialized or inspected.
    #[tracing::instrument(
        name = "tascarrel_host.repositories.prepare_versions",
        level = "info",
        skip(self),
        err
    )]
    pub async fn prepare_versions(&self) -> HostRepositoryResult<Vec<HostRepositoryVersion>> {
        let _operation = self.operation.lock().await;
        let sources = self.selected_sources(None)?;
        let states =
            futures_util::stream::iter(sources.into_iter().map(|(id, source)| async move {
                self.prepare_cache(&source).await.map(|state| (id, state))
            }))
            .buffer_unordered(DEFAULT_REFRESH_CONCURRENCY)
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<HostRepositoryResult<BTreeMap<_, _>>>()?;
        self.versions_from_states(None, &states)
    }

    /// Refreshes one configured cache or every cache concurrently and returns
    /// the resulting tracked-ref versions.
    ///
    /// # Errors
    ///
    /// Returns an error when a path is not configured or a selected upstream
    /// cannot be refreshed with the host Git environment.
    #[tracing::instrument(
        name = "tascarrel_host.repositories.refresh_versions",
        level = "info",
        skip(self, path),
        err
    )]
    pub async fn refresh_versions(
        &self,
        path: Option<&str>,
    ) -> HostRepositoryResult<Vec<HostRepositoryVersion>> {
        let _operation = self.operation.lock().await;
        let sources = self.selected_sources(path)?;
        let baselines = sources
            .keys()
            .map(|id| {
                let sequence = self
                    .cache_state_store_by_id(id)
                    .read()
                    .ok()
                    .map(|state| state.refresh_sequence);
                (id.clone(), sequence)
            })
            .collect::<BTreeMap<_, _>>();
        let states = futures_util::stream::iter(sources.into_iter().map(|(id, source)| {
            let baseline = baselines.get(&id).copied().flatten();
            async move {
                self.refresh_cache(&source, baseline)
                    .await
                    .map(|state| (id, state))
            }
        }))
        .buffer_unordered(DEFAULT_REFRESH_CONCURRENCY)
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<HostRepositoryResult<BTreeMap<_, _>>>()?;
        self.versions_from_states(path, &states)
    }

    /// Removes the isolated repository cache belonging to one deleted
    /// workspace.
    ///
    /// # Errors
    ///
    /// Returns an error when the repository state root or exact workspace
    /// cache path is unsafe, or the cache cannot be removed.
    #[tracing::instrument(
        name = "tascarrel_host.repositories.remove_workspace_cache",
        level = "info",
        skip_all,
        fields(workspace = %workspace),
        err
    )]
    pub async fn remove_workspace_cache(
        root: PathBuf,
        workspace: String,
    ) -> HostRepositoryResult<()> {
        let workspace_id = GitWorkspaceId::new(workspace).map_err(git_report)?;
        let metadata = match fs::symlink_metadata(&root) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(io_report(
                    format!("inspect repository state root {}", root.display()),
                    error,
                ));
            }
        };
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(unsafe_state(format!(
                "repository state root is unsafe: {}",
                root.display()
            )));
        }
        let workspace_caches = root.join("workspaces");
        let metadata = match fs::symlink_metadata(&workspace_caches) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(io_report(
                    format!(
                        "inspect workspace repository cache root {}",
                        workspace_caches.display()
                    ),
                    error,
                ));
            }
        };
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(unsafe_state(format!(
                "workspace repository cache root is unsafe: {}",
                workspace_caches.display()
            )));
        }
        let workspace_cache = workspace_caches.join(workspace_id.as_str());
        match fs::symlink_metadata(&workspace_cache) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                Err(unsafe_state(format!(
                    "workspace repository cache is unsafe: {}",
                    workspace_cache.display()
                )))
            }
            Ok(_) => {
                let display = workspace_cache.display().to_string();
                tokio::task::spawn_blocking(move || fs::remove_dir_all(&workspace_cache))
                    .await
                    .map_err(|error| {
                        error
                            .escalate(HostRepositoryError::Task)
                            .message("join workspace repository cache removal")
                    })?
                    .map_err(|error| {
                        io_report(
                            format!("remove workspace repository cache {display}"),
                            error,
                        )
                    })?;
                Ok(())
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(io_report(
                format!(
                    "inspect workspace repository cache {display}",
                    display = workspace_cache.display()
                ),
                error,
            )),
        }
    }

    /// Fetches one pod checkout into a durable, host-only reference.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid refs or a failed Git transport.
    #[tracing::instrument(
        name = "tascarrel_host.repositories.capture",
        level = "info",
        skip_all,
        fields(workspace = namespace, pod = %pod, repository = path, source = source_ref),
        err
    )]
    pub async fn capture(
        &self,
        channel: Channel,
        namespace: &str,
        pod: &PodId,
        path: &str,
        source_ref: &str,
    ) -> HostRepositoryResult<(String, String)> {
        let _operation = self.operation.lock().await;
        validate_path(path)?;
        let source_ref = SourceReference::new(source_ref).map_err(git_report)?;
        let workspace_id = GitWorkspaceId::new(namespace).map_err(git_report)?;
        let pod_id = GitPodId::new(&pod.0).map_err(git_report)?;
        let capture_id =
            CaptureId::new(uuid::Uuid::new_v4().simple().to_string()).map_err(git_report)?;
        let source = self
            .repository(path)
            .ok_or_else(|| invalid_request("repository path is not configured"))?;
        let _cache_lock = self.lock_store(source).await?;
        let store = self.open_store(source, false).await?;
        let captured = self
            .capture_channel(
                channel,
                store,
                &workspace_id,
                &pod_id,
                &source_ref,
                &capture_id,
            )
            .await?;
        Ok((
            captured.object.to_string(),
            captured.retained_as.to_string(),
        ))
    }

    /// Stages a captured branch for explicit user approval.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid branch, missing capture, or approval
    /// persistence failure.
    #[tracing::instrument(
        name = "tascarrel_host.repositories.request_branch_approval",
        level = "info",
        skip_all,
        fields(pod = %pod, repository = path, branch = ?branch),
        err
    )]
    pub async fn request_captured_branch_approval(
        &self,
        pod: &PodId,
        path: &str,
        reference: &str,
        branch: Option<&str>,
        force_with_lease: bool,
    ) -> HostRepositoryResult<RepositoryApprovalId> {
        let _operation = self.operation.lock().await;
        let source = self
            .repository(path)
            .ok_or_else(|| invalid_request("repository path is not configured"))?;
        let _cache_lock = self.lock_store(source).await?;
        let remote = Remote::new(source).map_err(git_report)?;
        let store = self.open_store(source, false).await?;
        let destination = match branch {
            Some(branch) => {
                validate_branch(branch)?;
                ReferenceName::new(format!("refs/heads/{branch}")).map_err(git_report)?
            }
            None => store
                .default_branch(&remote)
                .await
                .map_err(git_report)?
                .ok_or_else(missing_default_branch)?,
        };
        self.ensure_approval_permitted(path, source, &destination)?;
        self.stage_captured_approval(&store, pod, path, reference, destination, force_with_lease)
            .await
    }

    /// Stages a captured tag for explicit user approval.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid tag, missing capture, or approval
    /// persistence failure.
    #[tracing::instrument(
        name = "tascarrel_host.repositories.request_tag_approval",
        level = "info",
        skip_all,
        fields(pod = %pod, repository = path, tag = tag),
        err
    )]
    pub async fn request_captured_tag_approval(
        &self,
        pod: &PodId,
        path: &str,
        reference: &str,
        tag: &str,
        force_with_lease: bool,
    ) -> HostRepositoryResult<RepositoryApprovalId> {
        let _operation = self.operation.lock().await;
        let source = self
            .repository(path)
            .ok_or_else(|| invalid_request("repository path is not configured"))?;
        let _cache_lock = self.lock_store(source).await?;
        let store = self.open_store(source, false).await?;
        validate_tag(tag)?;
        let destination = ReferenceName::new(format!("refs/tags/{tag}")).map_err(git_report)?;
        self.ensure_approval_permitted(path, source, &destination)?;
        self.stage_captured_approval(&store, pod, path, reference, destination, force_with_lease)
            .await
    }

    /// Persists one captured ref together with its current upstream lease.
    async fn stage_captured_approval(
        &self,
        store: &RepositoryStore,
        pod: &PodId,
        path: &str,
        reference: &str,
        destination: ReferenceName,
        allow_rewrite: bool,
    ) -> HostRepositoryResult<RepositoryApprovalId> {
        let remote = Remote::new(
            self.repository(path)
                .ok_or_else(|| invalid_request("repository path is not configured"))?,
        )
        .map_err(git_report)?;
        let source = ReferenceName::new(reference).map_err(git_report)?;
        let state_store = self.cache_state_store(remote.as_str());
        let state = state_store.load_or_create().map_err(cache_state_report)?;
        let (refresh, _) = self
            .refresh_existing_store_locked(remote.as_str(), store, &state_store, state)
            .await?;
        let expected = refresh
            .references
            .into_iter()
            .find(|candidate| candidate.name == destination)
            .map(|candidate| candidate.object);
        let proposed = store
            .resolve_reference(&source)
            .await
            .map_err(git_report)?
            .object;
        let id = RepositoryApprovalId::generate();
        let approval = RepositoryApproval::new(
            id.clone(),
            pod.0.clone(),
            path.to_owned(),
            source_id(remote.as_str()),
            display_remote(remote.as_str()),
            vec![RepositoryApprovalUpdate {
                source: source.to_string(),
                destination: destination.to_string(),
                expected: expected.map(|object| object.to_string()),
                proposed: proposed.to_string(),
                allow_rewrite,
            }],
            None,
            None,
        );
        self.approvals.create(&approval).map_err(approval_report)?;
        Ok(id)
    }

    /// Pushes a previously captured pod commit to the configured upstream.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid branch, missing ref, or rejected push.
    #[tracing::instrument(
        name = "tascarrel_host.repositories.push_branch",
        level = "info",
        skip_all,
        fields(repository = path, branch = ?branch),
        err
    )]
    pub async fn push_captured(
        &self,
        path: &str,
        reference: &str,
        branch: Option<&str>,
        force_with_lease: bool,
    ) -> HostRepositoryResult<(String, String)> {
        let _operation = self.operation.lock().await;
        let source = self
            .repository(path)
            .ok_or_else(|| invalid_request("repository path is not configured"))?;
        let _cache_lock = self.lock_store(source).await?;
        let remote = Remote::new(source).map_err(git_report)?;
        let store = self.open_store(source, false).await?;
        let destination = match branch {
            Some(branch) => {
                validate_branch(branch)?;
                ReferenceName::new(format!("refs/heads/{branch}")).map_err(git_report)?
            }
            None => store
                .default_branch(&remote)
                .await
                .map_err(git_report)?
                .ok_or_else(missing_default_branch)?,
        };
        self.ensure_automatic_publication_allowed(path, source, &destination)?;
        let (object, destination) = self
            .publish_captured(store, &remote, reference, destination, force_with_lease)
            .await?;
        let branch = destination
            .as_str()
            .strip_prefix("refs/heads/")
            .ok_or_else(|| invalid_request("Git publication destination is not a branch"))?;
        Ok((object, branch.to_owned()))
    }

    /// Pushes a previously captured pod tag to the configured upstream.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid tag, missing ref, or rejected push.
    #[tracing::instrument(
        name = "tascarrel_host.repositories.push_tag",
        level = "info",
        skip_all,
        fields(repository = path, tag = tag),
        err
    )]
    pub async fn push_captured_tag(
        &self,
        path: &str,
        reference: &str,
        tag: &str,
        force_with_lease: bool,
    ) -> HostRepositoryResult<(String, String)> {
        let _operation = self.operation.lock().await;
        let source = self
            .repository(path)
            .ok_or_else(|| invalid_request("repository path is not configured"))?;
        let _cache_lock = self.lock_store(source).await?;
        let remote = Remote::new(source).map_err(git_report)?;
        let store = self.open_store(source, false).await?;
        validate_tag(tag)?;
        let destination = ReferenceName::new(format!("refs/tags/{tag}")).map_err(git_report)?;
        self.ensure_automatic_publication_allowed(path, source, &destination)?;
        let (object, destination) = self
            .publish_captured(store, &remote, reference, destination, force_with_lease)
            .await?;
        let tag = destination
            .as_str()
            .strip_prefix("refs/tags/")
            .ok_or_else(|| invalid_request("Git publication destination is not a tag"))?;
        Ok((object, tag.to_owned()))
    }

    async fn publish_captured(
        &self,
        store: RepositoryStore,
        remote: &Remote,
        reference: &str,
        destination: ReferenceName,
        force_with_lease: bool,
    ) -> HostRepositoryResult<(String, ReferenceName)> {
        let source_reference = ReferenceName::new(reference).map_err(git_report)?;
        let state_store = self.cache_state_store(remote.as_str());
        let state = state_store.load_or_create().map_err(cache_state_report)?;
        let (refresh, state) = self
            .refresh_existing_store_locked(remote.as_str(), &store, &state_store, state)
            .await?;
        let expected = refresh
            .references
            .into_iter()
            .find(|reference| reference.name == destination)
            .map(|reference| reference.object);
        let update = RefUpdate::new(
            source_reference,
            destination.clone(),
            expected,
            force_with_lease,
        )
        .map_err(git_report)?;
        let outcome = store.publish(remote, &[update]).await.map_err(git_report)?;
        self.refresh_existing_store_locked(remote.as_str(), &store, &state_store, state)
            .await?;
        let published = outcome
            .references
            .into_iter()
            .next()
            .ok_or_else(|| invalid_request("Git publication returned no reference"))?;
        Ok((published.object.to_string(), destination))
    }

    /// Durably claims one approval for background publication.
    pub(crate) fn claim_approval(
        &self,
        approval_id: &RepositoryApprovalId,
    ) -> HostRepositoryResult<Option<RepositoryApproval>> {
        self.approvals.claim(approval_id).map_err(approval_report)
    }

    /// Durably suppresses the automatic overlay for one pending approval.
    pub(crate) fn postpone_approval(
        &self,
        approval_id: &RepositoryApprovalId,
    ) -> HostRepositoryResult<()> {
        self.approvals
            .postpone(approval_id)
            .map_err(approval_report)
    }

    /// Returns every approval claimed before a previous host shutdown.
    pub(crate) fn claimed_approvals(&self) -> HostRepositoryResult<Vec<RepositoryApproval>> {
        self.approvals.claimed().map_err(approval_report)
    }

    /// Makes a failed background publication available for retry.
    pub(crate) fn fail_approval_publication(
        &self,
        approval_id: &RepositoryApprovalId,
        error: String,
    ) -> HostRepositoryResult<()> {
        self.approvals
            .fail_claim(approval_id, error)
            .map_err(approval_report)
    }

    /// Rejects one unclaimed approval without contacting its upstream.
    pub(crate) async fn reject_approval(
        &self,
        approval_id: &RepositoryApprovalId,
    ) -> HostRepositoryResult<RepositoryApproval> {
        let _operation = self.operation.lock().await;
        let approval = self.approvals.read(approval_id).map_err(approval_report)?;
        if approval.publishing {
            return Err(invalid_request(
                "approval is already being published in the background",
            ));
        }
        self.remove_approval_namespace(&approval).await?;
        if let Some(push_id) = &approval.push_id {
            self.transition_push_status(push_id, RepositoryPushState::Rejected)?;
        }
        self.approvals
            .remove(approval_id)
            .map_err(approval_report)?;
        Ok(approval)
    }

    /// Publishes one durably claimed approval against its exact captured lease.
    pub(crate) async fn publish_claimed_approval(
        &self,
        approval_id: &RepositoryApprovalId,
    ) -> HostRepositoryResult<RepositoryApproval> {
        let _operation = self.operation.lock().await;
        let approval = self.approvals.read(approval_id).map_err(approval_report)?;
        if !approval.publishing {
            return Err(invalid_request(
                "approval is not claimed for background publication",
            ));
        }
        let current = self
            .currently_configured_repository(&approval.path, None)?
            .ok_or_else(|| invalid_request("approval repository is no longer configured"))?;
        if source_id(&current.source) != approval.repository_id {
            return Err(invalid_request(
                "approval repository is no longer configured with the staged upstream",
            ));
        }
        for update in &approval.updates {
            let destination = ReferenceName::new(&update.destination).map_err(git_report)?;
            if current.policy.reference_policy(&destination) == RepositoryPushPolicy::Deny {
                return Err(invalid_request(
                    "approval is denied by the current Git publication policy",
                ));
            }
        }
        let _cache_lock = self.lock_store_by_id(&approval.repository_id).await?;
        let cache_path = self.cache_path_by_id(&approval.repository_id);
        let store =
            RepositoryStore::open_existing(self.git.clone(), cache_path).map_err(git_report)?;
        let remote = Remote::new(&current.source).map_err(git_report)?;
        let mut updates = Vec::with_capacity(approval.updates.len());
        for stored in &approval.updates {
            let source = ReferenceName::new(&stored.source).map_err(git_report)?;
            let destination = ReferenceName::new(&stored.destination).map_err(git_report)?;
            let retained = store.resolve_reference(&source).await.map_err(git_report)?;
            let proposed = ObjectId::new(&stored.proposed).map_err(git_report)?;
            if retained.object != proposed {
                return Err(invalid_request(
                    "retained approval object no longer matches its request",
                ));
            }
            let expected = stored
                .expected
                .as_deref()
                .map(ObjectId::new)
                .transpose()
                .map_err(git_report)?;
            updates.push(
                RefUpdate::new(source, destination, expected, stored.allow_rewrite)
                    .map_err(git_report)?,
            );
        }
        store.publish(&remote, &updates).await.map_err(git_report)?;
        let state_store = self.cache_state_store_by_id(&approval.repository_id);
        let refresh = async {
            let state = state_store.load_or_create().map_err(cache_state_report)?;
            self.refresh_existing_store_locked(remote.as_str(), &store, &state_store, state)
                .await?;
            Ok::<(), Report<HostRepositoryError>>(())
        }
        .await;
        if let Err(error) = refresh {
            warn!(approval_id = %approval.id.0, %error, "could not refresh cache state after approved publication");
        }
        if let Some(push_id) = &approval.push_id {
            self.transition_push_status(push_id, RepositoryPushState::Published)?;
        }
        if let Some(namespace) = &approval.receive_namespace {
            let namespace = ReceiveNamespace::new(namespace).map_err(git_report)?;
            if let Err(error) = store.remove_receive_namespace(&namespace).await {
                warn!(approval_id = %approval.id.0, %error, "could not remove published approval namespace");
            }
        }
        if let Err(error) = self.approvals.complete_claim(approval_id) {
            warn!(approval_id = %approval.id.0, %error, "could not remove published approval state");
        }
        Ok(approval)
    }

    /// Removes namespaced refs retained by a rejected receive-pack request.
    async fn remove_approval_namespace(
        &self,
        approval: &RepositoryApproval,
    ) -> HostRepositoryResult<()> {
        let Some(namespace) = &approval.receive_namespace else {
            return Ok(());
        };
        let cache_path = self.cache_path_by_id(&approval.repository_id);
        let metadata = match fs::symlink_metadata(&cache_path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(io_report(
                    format!("inspect approval repository cache {}", cache_path.display()),
                    error,
                ));
            }
        };
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(unsafe_state(format!(
                "approval repository cache is unsafe: {}",
                cache_path.display()
            )));
        }
        let _cache_lock = self.lock_store_by_id(&approval.repository_id).await?;
        let store =
            RepositoryStore::open_existing(self.git.clone(), cache_path).map_err(git_report)?;
        let namespace = ReceiveNamespace::new(namespace).map_err(git_report)?;
        store
            .remove_receive_namespace(&namespace)
            .await
            .map_err(git_report)?;
        Ok(())
    }

    /// Serves upload-pack or namespaced receive-pack for a configured cache.
    ///
    /// # Errors
    ///
    /// Returns an error when framing, mirror preparation, or Git fails.
    #[tracing::instrument(
        name = "tascarrel_host.repositories.upload_pack",
        level = "debug",
        skip(self, channel),
        err
    )]
    pub async fn serve_upload_pack(self: Arc<Self>, channel: Channel) -> HostRepositoryResult<()> {
        let mut framed = Framed::new(channel);
        let request = framed
            .read::<GitHostRequest>()
            .await
            .map_err(protocol_report)?
            .ok_or_else(|| invalid_request("Git channel closed before its request"))?;
        let source = match &request {
            GitHostRequest::UploadPack { source, .. }
            | GitHostRequest::ReceivePack { source, .. } => source,
        };
        let source_configured = match self.source_is_currently_configured(source) {
            Ok(configured) => configured,
            Err(report) => return reject_git_preparation(&mut framed, report).await,
        };
        if !source_configured {
            framed
                .write(&GitOpenResponse::Error {
                    error: RemoteError::new(
                        ErrorCode::PermissionDenied,
                        "repository is not configured",
                    ),
                })
                .await
                .map_err(protocol_report)?;
            return Ok(());
        }
        match request {
            GitHostRequest::UploadPack {
                source,
                refresh,
                expected_cache_id,
                expected_version,
            } => {
                self.serve_fetch(framed, source, refresh, expected_cache_id, expected_version)
                    .await
            }
            GitHostRequest::ReceivePack {
                source,
                pod_id,
                path,
            } => {
                let path_configured =
                    match self.source_is_currently_configured_for_path(&path, &source) {
                        Ok(configured) => configured,
                        Err(report) => return reject_git_preparation(&mut framed, report).await,
                    };
                if !path_configured {
                    framed
                        .write(&GitOpenResponse::Error {
                            error: RemoteError::new(
                                ErrorCode::PermissionDenied,
                                "repository path is not configured for this source",
                            ),
                        })
                        .await
                        .map_err(protocol_report)?;
                    return Ok(());
                }
                self.serve_receive_pack(framed, source, pod_id, path).await
            }
        }
    }

    async fn serve_fetch(
        &self,
        mut framed: Framed<Channel>,
        source: String,
        refresh: bool,
        expected_cache_id: Option<String>,
        expected_version: Option<u64>,
    ) -> HostRepositoryResult<()> {
        let versioned = expected_cache_id.is_some();
        if expected_cache_id.is_some() != expected_version.is_some() {
            framed
                .write(&GitOpenResponse::Error {
                    error: RemoteError::new(
                        ErrorCode::InvalidRequest,
                        "cache identity and version must be requested together",
                    ),
                })
                .await
                .map_err(protocol_report)?;
            return Ok(());
        }
        let state_store = self.cache_state_store(&source);
        let baseline = state_store.read().ok().map(|state| state.refresh_sequence);
        let _cache_lock = match self.lock_store(&source).await {
            Ok(lock) => lock,
            Err(report) => return reject_git_preparation(&mut framed, report).await,
        };
        let mut state = if self.cache_path(&source).exists() {
            match state_store.load_or_create().map_err(cache_state_report) {
                Ok(state) => state,
                Err(report) => return reject_git_preparation(&mut framed, report).await,
            }
        } else {
            let state = RepositoryCacheState::new();
            if let Err(report) = state_store.write(&state).map_err(cache_state_report) {
                return reject_git_preparation(&mut framed, report).await;
            }
            state
        };
        if refresh && (state.version == 0 || baseline == Some(state.refresh_sequence)) {
            state = match self
                .refresh_store_locked(&source, &state_store, state)
                .await
            {
                Ok(state) => state,
                Err(report) => return reject_git_preparation(&mut framed, report).await,
            };
        }
        if let (Some(expected_cache_id), Some(expected_version)) =
            (expected_cache_id, expected_version)
            && (state.id.0.as_ref() != expected_cache_id || state.version != expected_version)
        {
            framed
                .write(&GitOpenResponse::Error {
                    error: RemoteError::new(
                        ErrorCode::Busy,
                        "repository cache advanced before the requested version was served",
                    ),
                })
                .await
                .map_err(protocol_report)?;
            return Ok(());
        }
        if state.version == 0 {
            framed
                .write(&GitOpenResponse::Error {
                    error: RemoteError::new(
                        ErrorCode::ExecutionFailed,
                        "repository cache has no successful upstream snapshot",
                    ),
                })
                .await
                .map_err(protocol_report)?;
            return Ok(());
        }
        let store = match RepositoryStore::open_existing(self.git.clone(), self.cache_path(&source))
            .map_err(git_report)
        {
            Ok(store) => store,
            Err(report) => return reject_git_preparation(&mut framed, report).await,
        };
        let upload_pack = match store.upload_pack().map_err(git_report) {
            Ok(upload_pack) => upload_pack,
            Err(report) => return reject_git_preparation(&mut framed, report).await,
        };
        let default_branch = if versioned {
            match store.cached_default_branch().await.map_err(git_report) {
                Ok(default_branch) => default_branch.map(|branch| branch.to_string()),
                Err(report) => return reject_git_preparation(&mut framed, report).await,
            }
        } else {
            None
        };
        let response = if versioned {
            GitOpenResponse::VersionedReady { default_branch }
        } else {
            GitOpenResponse::Ready
        };
        framed.write(&response).await.map_err(protocol_report)?;
        upload_pack
            .relay(framed.into_inner())
            .await
            .map_err(git_report)
    }

    /// Runs receive-pack in a fresh namespace and durably records its changes.
    async fn serve_receive_pack(
        &self,
        mut framed: Framed<Channel>,
        source: String,
        pod_id: PodId,
        path: String,
    ) -> HostRepositoryResult<()> {
        let _operation = self.operation.lock().await;
        let _cache_lock = match self.lock_store(&source).await {
            Ok(lock) => lock,
            Err(report) => return reject_git_preparation(&mut framed, report).await,
        };
        let store = match self.open_store(&source, true).await {
            Ok(store) => store,
            Err(report) => return reject_git_preparation(&mut framed, report).await,
        };
        let push_id = RepositoryPushId::generate();
        let namespace = match ReceiveNamespace::new(push_id.0.to_string()).map_err(git_report) {
            Ok(namespace) => namespace,
            Err(report) => return reject_git_preparation(&mut framed, report).await,
        };
        let baseline = match store.stage_receive_namespace(&namespace).await {
            Ok(baseline) => baseline,
            Err(report) => {
                if let Err(cleanup) = store.remove_receive_namespace(&namespace).await {
                    warn!(push_id = %push_id.0, error = %cleanup, "could not remove incomplete receive-pack namespace");
                }
                return reject_git_preparation(&mut framed, git_report(report)).await;
            }
        };
        let receive_pack = match store.receive_pack(&namespace) {
            Ok(receive_pack) => receive_pack,
            Err(report) => {
                if let Err(cleanup) = store.remove_receive_namespace(&namespace).await {
                    warn!(push_id = %push_id.0, error = %cleanup, "could not remove incomplete receive-pack namespace");
                }
                return reject_git_preparation(&mut framed, git_report(report)).await;
            }
        };
        framed
            .write(&GitOpenResponse::ReceivePackReady {
                push_id: push_id.0.to_string(),
            })
            .await
            .map_err(protocol_report)?;
        let relay = receive_pack.relay_retained(framed.into_inner()).await;
        let mut channel = match relay {
            Ok(channel) => channel,
            Err(report) => {
                self.finish_received_push(
                    &store,
                    &namespace,
                    &push_id,
                    &pod_id,
                    RepositoryPushState::Failed(bounded_git_error(&report)),
                )
                .await?;
                return Err(git_report(report));
            }
        };
        let received = match store.received_updates(&namespace, &baseline).await {
            Ok(received) => received,
            Err(report) => {
                let result = self
                    .finish_received_push(
                        &store,
                        &namespace,
                        &push_id,
                        &pod_id,
                        RepositoryPushState::Failed(bounded_git_error(&report)),
                    )
                    .await;
                channel
                    .shutdown()
                    .await
                    .map_err(|error| io_report("close Git receive-pack channel", error))?;
                return result;
            }
        };
        let result = self
            .complete_received_push(
                &store, &namespace, &source, &pod_id, &path, &push_id, &received,
            )
            .await;
        channel
            .shutdown()
            .await
            .map_err(|error| io_report("close Git receive-pack channel", error))?;
        result
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "the arguments are the authenticated context of one completed push"
    )]
    async fn complete_received_push(
        &self,
        store: &RepositoryStore,
        namespace: &ReceiveNamespace,
        source: &str,
        pod_id: &PodId,
        path: &str,
        push_id: &RepositoryPushId,
        received: &[ReceivedReferenceUpdate],
    ) -> HostRepositoryResult<()> {
        if received.is_empty() {
            return self
                .finish_received_push(
                    store,
                    namespace,
                    push_id,
                    pod_id,
                    RepositoryPushState::Published,
                )
                .await;
        }
        let current = match self.currently_configured_repository(path, Some(source)) {
            Ok(Some(current)) => current,
            Ok(None) => {
                return self
                    .finish_received_push(
                        store,
                        namespace,
                        push_id,
                        pod_id,
                        RepositoryPushState::Failed(
                            "repository path is no longer configured for this source".to_owned(),
                        ),
                    )
                    .await;
            }
            Err(report) => {
                return self
                    .finish_received_push(
                        store,
                        namespace,
                        push_id,
                        pod_id,
                        RepositoryPushState::Failed(bounded_git_error(&report)),
                    )
                    .await;
            }
        };
        match current
            .policy
            .updates_policy(received.iter().map(|update| &update.destination))
        {
            RepositoryPushPolicy::Deny => {
                self.finish_received_push(
                    store,
                    namespace,
                    push_id,
                    pod_id,
                    RepositoryPushState::Denied(
                        "push denied by configured Git publication policy".to_owned(),
                    ),
                )
                .await
            }
            RepositoryPushPolicy::RequireApproval => {
                self.retain_received_push_for_approval(
                    store, namespace, source, pod_id, path, push_id, received,
                )
                .await
            }
            RepositoryPushPolicy::Allow => {
                self.publish_received_push(store, namespace, source, pod_id, push_id, received)
                    .await
            }
        }
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "the arguments are the authenticated context retained by an approval"
    )]
    async fn retain_received_push_for_approval(
        &self,
        store: &RepositoryStore,
        namespace: &ReceiveNamespace,
        source: &str,
        pod_id: &PodId,
        path: &str,
        push_id: &RepositoryPushId,
        received: &[ReceivedReferenceUpdate],
    ) -> HostRepositoryResult<()> {
        let approval_id = RepositoryApprovalId::generate();
        let updates = received.iter().map(repository_approval_update).collect();
        let approval = RepositoryApproval::new(
            approval_id.clone(),
            pod_id.0.clone(),
            path.to_owned(),
            source_id(source),
            display_remote(source),
            updates,
            Some(push_id.clone()),
            Some(namespace.to_string()),
        );
        self.record_push_status(
            push_id,
            pod_id,
            RepositoryPushState::ApprovalRequired(approval_id),
        )?;
        if let Err(report) = self.approvals.create(&approval) {
            if let Err(cleanup) = store.remove_receive_namespace(namespace).await {
                warn!(approval_id = %approval.id.0, error = %cleanup, "could not remove unrecorded approval namespace");
            }
            self.transition_push_status(
                push_id,
                RepositoryPushState::Failed(bounded_git_error(&report)),
            )?;
        }
        Ok(())
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "the arguments are the authenticated context of one automatic publication"
    )]
    async fn publish_received_push(
        &self,
        store: &RepositoryStore,
        namespace: &ReceiveNamespace,
        source: &str,
        pod_id: &PodId,
        push_id: &RepositoryPushId,
        received: &[ReceivedReferenceUpdate],
    ) -> HostRepositoryResult<()> {
        let updates = match received
            .iter()
            .map(|update| {
                RefUpdate::new(
                    update.source.clone(),
                    update.destination.clone(),
                    update.previous.clone(),
                    update.rewrites,
                )
            })
            .collect::<Result<Vec<_>, _>>()
        {
            Ok(updates) => updates,
            Err(report) => {
                return self
                    .finish_received_push(
                        store,
                        namespace,
                        push_id,
                        pod_id,
                        RepositoryPushState::Failed(bounded_git_error(&report)),
                    )
                    .await;
            }
        };
        let remote = match Remote::new(source) {
            Ok(remote) => remote,
            Err(report) => {
                return self
                    .finish_received_push(
                        store,
                        namespace,
                        push_id,
                        pod_id,
                        RepositoryPushState::Failed(bounded_git_error(&report)),
                    )
                    .await;
            }
        };
        match store.publish(&remote, &updates).await {
            Ok(_) => {
                let state_store = self.cache_state_store(source);
                let refresh = async {
                    let state = state_store.load_or_create().map_err(cache_state_report)?;
                    self.refresh_existing_store_locked(remote.as_str(), store, &state_store, state)
                        .await?;
                    Ok::<(), Report<HostRepositoryError>>(())
                }
                .await;
                if let Err(error) = refresh {
                    warn!(push_id = %push_id.0, %error, "could not refresh cache state after automatic publication");
                }
                self.finish_received_push(
                    store,
                    namespace,
                    push_id,
                    pod_id,
                    RepositoryPushState::Published,
                )
                .await
            }
            Err(report) => {
                self.finish_received_push(
                    store,
                    namespace,
                    push_id,
                    pod_id,
                    RepositoryPushState::Failed(bounded_git_error(&report)),
                )
                .await
            }
        }
    }

    async fn finish_received_push(
        &self,
        store: &RepositoryStore,
        namespace: &ReceiveNamespace,
        push_id: &RepositoryPushId,
        pod_id: &PodId,
        state: RepositoryPushState,
    ) -> HostRepositoryResult<()> {
        if let Err(cleanup) = store.remove_receive_namespace(namespace).await {
            warn!(push_id = %push_id.0, error = %cleanup, "could not remove completed receive-pack namespace");
        }
        self.record_push_status(push_id, pod_id, state)
    }

    fn source_is_currently_configured(&self, source: &str) -> HostRepositoryResult<bool> {
        Ok(load_workspace_config(&self.config)?
            .repos
            .values()
            .any(|repository| repository.source == source))
    }

    /// Revalidates an exact path-to-source binding against current policy.
    fn source_is_currently_configured_for_path(
        &self,
        path: &str,
        source: &str,
    ) -> HostRepositoryResult<bool> {
        self.currently_configured_repository(path, Some(source))
            .map(|repository| repository.is_some())
    }

    fn currently_configured_repository(
        &self,
        path: &str,
        source: Option<&str>,
    ) -> HostRepositoryResult<Option<HostRepository>> {
        Ok(load_workspace_config(&self.config)?
            .repos
            .get(path)
            .filter(|repository| source.is_none_or(|source| repository.source == source))
            .cloned())
    }

    fn ensure_approval_permitted(
        &self,
        path: &str,
        source: &str,
        destination: &ReferenceName,
    ) -> HostRepositoryResult<()> {
        let repository = self
            .currently_configured_repository(path, Some(source))?
            .ok_or_else(|| invalid_request("repository is no longer configured"))?;
        if repository.policy.reference_policy(destination) == RepositoryPushPolicy::Deny {
            return Err(invalid_request(
                "push denied by configured Git publication policy",
            ));
        }
        Ok(())
    }

    fn ensure_automatic_publication_allowed(
        &self,
        path: &str,
        source: &str,
        destination: &ReferenceName,
    ) -> HostRepositoryResult<()> {
        let repository = self
            .currently_configured_repository(path, Some(source))?
            .ok_or_else(|| invalid_request("repository is no longer configured"))?;
        match repository.policy.reference_policy(destination) {
            RepositoryPushPolicy::Allow => Ok(()),
            RepositoryPushPolicy::RequireApproval => Err(invalid_request(
                "push requires approval under configured Git publication policy",
            )),
            RepositoryPushPolicy::Deny => Err(invalid_request(
                "push denied by configured Git publication policy",
            )),
        }
    }

    fn record_push_status(
        &self,
        push_id: &RepositoryPushId,
        pod_id: &PodId,
        state: RepositoryPushState,
    ) -> HostRepositoryResult<()> {
        self.push_statuses
            .create(&RepositoryPushStatus::new(
                push_id.clone(),
                pod_id.0.clone(),
                state,
            ))
            .map_err(push_status_report)
    }

    fn transition_push_status(
        &self,
        push_id: &RepositoryPushId,
        state: RepositoryPushState,
    ) -> HostRepositoryResult<()> {
        self.push_statuses
            .transition(push_id, state)
            .map_err(push_status_report)
    }

    async fn capture_channel(
        &self,
        mut channel: Channel,
        store: RepositoryStore,
        workspace_id: &GitWorkspaceId,
        pod_id: &GitPodId,
        source_ref: &SourceReference,
        capture_id: &CaptureId,
    ) -> HostRepositoryResult<CapturedReference> {
        let socket = self.root.join(format!("git-{}.sock", uuid::Uuid::new_v4()));
        let listener = tokio::net::UnixListener::bind(&socket).map_err(|error| {
            io_report(
                format!("bind Git helper socket {}", socket.display()),
                error,
            )
        })?;
        fs::set_permissions(&socket, fs::Permissions::from_mode(0o600)).map_err(|error| {
            io_report(
                format!("set Git helper socket permissions {}", socket.display()),
                error,
            )
        })?;
        let helper_path = helper_search_path(&self.root)?;
        let git = self
            .git
            .clone()
            .with_environment("TASCARREL_GIT_SOCKET", socket.as_os_str())
            .with_environment("PATH", helper_path);
        let store = RepositoryStore::open(git, store.path().to_owned())
            .await
            .map_err(git_report)?;
        let remote = Remote::new("tascarrel://pod").map_err(git_report)?;
        let import = store.import_capture(&remote, workspace_id, pod_id, source_ref, capture_id);
        tokio::pin!(import);
        let (mut helper, _) = tokio::select! {
            accepted = listener.accept() => accepted.map_err(|error| io_report("accept Git remote helper", error))?,
            result = &mut import => {
                if let Err(error) = fs::remove_file(&socket) {
                    warn!(path = %socket.display(), %error, "could not remove Git helper socket");
                }
                return result.map_err(git_report);
            }
        };
        let relay = tokio::io::copy_bidirectional(&mut helper, &mut channel);
        let (relay, captured) = tokio::join!(relay, import);
        drop(helper);
        drop(channel);
        if let Err(error) = fs::remove_file(&socket) {
            warn!(path = %socket.display(), %error, "could not remove Git helper socket");
        }
        relay.map_err(|error| io_report("relay captured repository", error))?;
        captured.map_err(git_report)
    }

    fn selected_sources(
        &self,
        path: Option<&str>,
    ) -> HostRepositoryResult<BTreeMap<String, String>> {
        if let Some(path) = path {
            validate_path(path)?;
            let repository = self
                .repositories
                .get(path)
                .ok_or_else(|| invalid_request("repository path is not configured"))?;
            return Ok(BTreeMap::from([(
                source_id(&repository.source),
                repository.source.clone(),
            )]));
        }
        Ok(self
            .repositories
            .values()
            .map(|repository| (source_id(&repository.source), repository.source.clone()))
            .collect())
    }

    fn versions_from_states(
        &self,
        path: Option<&str>,
        states: &BTreeMap<String, RepositoryCacheState>,
    ) -> HostRepositoryResult<Vec<HostRepositoryVersion>> {
        self.repositories
            .iter()
            .filter(|(configured_path, _)| path.is_none_or(|path| path == configured_path.as_str()))
            .map(|(path, repository)| {
                let state = states
                    .get(&source_id(&repository.source))
                    .ok_or_else(|| unsafe_state("prepared repository cache state is missing"))?;
                Ok(HostRepositoryVersion {
                    path: path.clone(),
                    cache_id: state.id.clone(),
                    version: state.version,
                    updated_at: state.version_updated_at.ok_or_else(|| {
                        unsafe_state("prepared repository cache has no version timestamp")
                    })?,
                })
            })
            .collect()
    }

    async fn prepare_cache(&self, source: &str) -> HostRepositoryResult<RepositoryCacheState> {
        let _cache_lock = self.lock_store(source).await?;
        let cache_path = self.cache_path(source);
        let state_store = self.cache_state_store(source);
        let mut state = if cache_path.exists() {
            state_store.load_or_create().map_err(cache_state_report)?
        } else {
            let state = RepositoryCacheState::new();
            state_store.write(&state).map_err(cache_state_report)?;
            state
        };
        if state.version == 0 {
            state = self
                .refresh_store_locked(source, &state_store, state)
                .await?;
        } else {
            RepositoryStore::open_existing(self.git.clone(), cache_path).map_err(git_report)?;
        }
        Ok(state)
    }

    async fn refresh_cache(
        &self,
        source: &str,
        baseline: Option<u64>,
    ) -> HostRepositoryResult<RepositoryCacheState> {
        let _cache_lock = self.lock_store(source).await?;
        let cache_path = self.cache_path(source);
        let state_store = self.cache_state_store(source);
        let state = if cache_path.exists() {
            state_store.load_or_create().map_err(cache_state_report)?
        } else {
            let state = RepositoryCacheState::new();
            state_store.write(&state).map_err(cache_state_report)?;
            state
        };
        if state.version > 0 && baseline != Some(state.refresh_sequence) {
            return Ok(state);
        }
        self.refresh_store_locked(source, &state_store, state).await
    }

    async fn refresh_store_locked(
        &self,
        source: &str,
        state_store: &RepositoryCacheStateStore,
        mut state: RepositoryCacheState,
    ) -> HostRepositoryResult<RepositoryCacheState> {
        let store = match RepositoryStore::open(self.git.clone(), self.cache_path(source)).await {
            Ok(store) => store,
            Err(report) => {
                let report = git_report(report);
                Self::record_refresh_failure(state_store, &mut state, &report);
                return Err(report);
            }
        };
        self.refresh_existing_store_locked(source, &store, state_store, state)
            .await
            .map(|(_, state)| state)
    }

    async fn refresh_existing_store_locked(
        &self,
        source: &str,
        store: &RepositoryStore,
        state_store: &RepositoryCacheStateStore,
        mut state: RepositoryCacheState,
    ) -> HostRepositoryResult<(RepositoryRefresh, RepositoryCacheState)> {
        let remote = match Remote::new(source) {
            Ok(remote) => remote,
            Err(report) => {
                let report = git_report(report);
                Self::record_refresh_failure(state_store, &mut state, &report);
                return Err(report);
            }
        };
        let refresh = match store.refresh_snapshot(&remote).await {
            Ok(refresh) => refresh,
            Err(report) => {
                let report = git_report(report);
                Self::record_refresh_failure(state_store, &mut state, &report);
                return Err(report);
            }
        };
        state
            .refreshed(tracked_refs_digest(&refresh)?)
            .map_err(cache_state_report)?;
        state_store.write(&state).map_err(cache_state_report)?;
        Ok((refresh, state))
    }

    fn record_refresh_failure(
        state_store: &RepositoryCacheStateStore,
        state: &mut RepositoryCacheState,
        report: &Report<HostRepositoryError>,
    ) {
        state.failed(bounded_git_error(report));
        if let Err(error) = state_store.write(state) {
            warn!(%error, "could not persist repository cache refresh failure");
        }
    }

    async fn open_store(
        &self,
        source: &str,
        refresh: bool,
    ) -> HostRepositoryResult<RepositoryStore> {
        let path = self.cache_path(source);
        if refresh {
            let state_store = self.cache_state_store(source);
            let state = if path.exists() {
                state_store.load_or_create().map_err(cache_state_report)?
            } else {
                let state = RepositoryCacheState::new();
                state_store.write(&state).map_err(cache_state_report)?;
                state
            };
            self.refresh_store_locked(source, &state_store, state)
                .await?;
            return RepositoryStore::open_existing(self.git.clone(), path).map_err(git_report);
        }
        RepositoryStore::open(self.git.clone(), path)
            .await
            .map_err(git_report)
    }

    async fn lock_store(&self, source: &str) -> HostRepositoryResult<std::fs::File> {
        self.lock_store_by_id(&source_id(source)).await
    }

    async fn lock_store_by_id(&self, id: &str) -> HostRepositoryResult<std::fs::File> {
        let lock_path = self.root.join(format!("{id}.lock"));
        tokio::task::spawn_blocking(move || {
            let lock = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .open(&lock_path)
                .map_err(|error| {
                    io_report(
                        format!("open repository lock {}", lock_path.display()),
                        error,
                    )
                })?;
            lock.lock_exclusive()
                .map_err(|error| io_report("lock host repository mirror", error))?;
            Ok(lock)
        })
        .await
        .map_err(|error| {
            error
                .escalate(HostRepositoryError::Task)
                .message("join repository lock acquisition")
        })?
    }

    fn cache_path(&self, source: &str) -> PathBuf {
        self.cache_path_by_id(&source_id(source))
    }

    fn cache_path_by_id(&self, id: &str) -> PathBuf {
        self.root.join(format!("{id}.git"))
    }

    fn cache_state_store(&self, source: &str) -> RepositoryCacheStateStore {
        self.cache_state_store_by_id(&source_id(source))
    }

    fn cache_state_store_by_id(&self, id: &str) -> RepositoryCacheStateStore {
        RepositoryCacheStateStore::new(&self.root, id)
    }
}

/// Exact tracked-ref version of one configured host cache.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostRepositoryVersion {
    /// Configured destination path below `/workspace`.
    pub path: String,
    /// Stable workspace-isolated cache identity.
    pub cache_id: tascarrel_api::ids::RepositoryCacheId,
    /// Monotonic tracked-ref version.
    pub version: u64,
    /// Time at which tracked refs changed and created this version.
    pub updated_at: Timestamp,
}

/// One configured repository and the state of its workspace object cache.
#[derive(Clone, Debug)]
pub(crate) struct HostRepositoryStatus {
    /// Destination path below `/workspace`.
    pub(crate) path: String,
    /// Display-safe upstream source with credential-bearing URL parts removed.
    pub(crate) source: String,
    /// Current cache state observed without upstream I/O.
    pub(crate) cache: HostRepositoryCache,
}

/// Current state of one configured repository cache.
#[derive(Clone, Debug)]
pub(crate) enum HostRepositoryCache {
    /// The cache has not been initialized yet.
    Missing,
    /// The cache is usable and has bounded Git statistics.
    Ready(HostRepositoryCacheReady),
    /// The cache exists but could not be inspected.
    Failed(String),
}

/// Local statistics and durable refresh state for one usable cache.
#[derive(Clone, Debug)]
pub(crate) struct HostRepositoryCacheReady {
    /// Bounded object and reference statistics.
    pub(crate) statistics: RepositoryStatistics,
    /// Durable identity and tracked-ref version.
    pub(crate) state: RepositoryCacheState,
}

/// Failure while enforcing repository policy or operating host-owned state.
#[derive(Debug, Error)]
pub enum HostRepositoryError {
    /// A caller supplied an invalid repository operation or identifier.
    #[error("invalid repository request")]
    InvalidRequest,
    /// Workspace repository configuration could not be loaded or validated.
    #[error("failed to load repository configuration")]
    InvalidConfiguration,
    /// Repository state did not satisfy the required filesystem invariants.
    #[error("repository state is unsafe")]
    UnsafeState,
    /// A filesystem or local socket operation failed.
    #[error("repository I/O failed")]
    Io,
    /// An asynchronous repository task failed.
    #[error("repository task failed")]
    Task,
    /// Repository protocol framing or transport failed.
    #[error("repository protocol failed")]
    Protocol,
    /// A managed Git operation failed.
    #[error("Git repository operation failed")]
    Git,
    /// Durable repository approval state could not be read or updated.
    #[error("repository approval state failed")]
    Approval,
    /// Durable push status could not be read or updated.
    #[error("repository push status failed")]
    PushStatus,
    /// Durable repository cache identity or version state could not be used.
    #[error("repository cache state failed")]
    CacheState,
}

/// Result of a host-owned repository operation.
pub type HostRepositoryResult<T> = Result<T, Report<HostRepositoryError>>;

#[derive(Clone, Default)]
struct HostWorkspaceConfig {
    repos: BTreeMap<String, HostRepository>,
}

#[derive(Clone)]
struct HostRepository {
    source: String,
    policy: RepositoryPolicy,
}

fn load_workspace_config(config: &Path) -> HostRepositoryResult<HostWorkspaceConfig> {
    let config_file = load_config_file(config, DEFAULT_MAX_CONFIG_BYTES)
        .map_err(|report| report.escalate(HostRepositoryError::InvalidConfiguration))?;
    let workspace_policy =
        RepositoryPolicy::from_config(config_file.git.as_ref()).map_err(policy_report)?;
    let parsed = HostWorkspaceConfig {
        repos: config_file
            .repos
            .unwrap_or_default()
            .into_iter()
            .map(|(path, repository)| {
                let policy = repository
                    .git
                    .as_ref()
                    .map_or_else(
                        || Ok(workspace_policy.clone()),
                        |config| RepositoryPolicy::from_config(Some(config)),
                    )
                    .map_err(policy_report)?;
                Ok((
                    path.into(),
                    HostRepository {
                        source: repository.source.into(),
                        policy,
                    },
                ))
            })
            .collect::<HostRepositoryResult<_>>()?,
    };
    for (path, repository) in &parsed.repos {
        validate_path(path)?;
        validate_source(&repository.source)?;
    }
    Ok(parsed)
}

fn invalid_request(message: impl Into<String>) -> Report<HostRepositoryError> {
    HostRepositoryError::InvalidRequest
        .report()
        .message(message.into())
}

fn missing_default_branch() -> Report<HostRepositoryError> {
    invalid_request("the upstream has no advertised default branch; specify a branch")
}

fn invalid_configuration(message: impl Into<String>) -> Report<HostRepositoryError> {
    HostRepositoryError::InvalidConfiguration
        .report()
        .message(message.into())
}

fn unsafe_state(message: impl Into<String>) -> Report<HostRepositoryError> {
    HostRepositoryError::UnsafeState
        .report()
        .message(message.into())
}

fn io_report(action: impl Into<String>, error: io::Error) -> Report<HostRepositoryError> {
    error
        .escalate(HostRepositoryError::Io)
        .message(action.into())
}

fn git_report(report: reportify::Report<tascarrel_git::GitError>) -> Report<HostRepositoryError> {
    report.escalate(HostRepositoryError::Git)
}

fn approval_report(report: Report<RepositoryApprovalStoreError>) -> Report<HostRepositoryError> {
    report.escalate(HostRepositoryError::Approval)
}

fn policy_report(report: Report<RepositoryPolicyError>) -> Report<HostRepositoryError> {
    report.escalate(HostRepositoryError::InvalidConfiguration)
}

fn push_status_report(
    report: Report<RepositoryPushStatusStoreError>,
) -> Report<HostRepositoryError> {
    report.escalate(HostRepositoryError::PushStatus)
}

fn cache_state_report(report: Report<RepositoryCacheStateError>) -> Report<HostRepositoryError> {
    report.escalate(HostRepositoryError::CacheState)
}

fn protocol_report(error: CodecError) -> Report<HostRepositoryError> {
    error.escalate(HostRepositoryError::Protocol)
}

/// Sends a safe handshake rejection while preserving the internal error report.
async fn reject_git_preparation(
    framed: &mut Framed<Channel>,
    report: Report<HostRepositoryError>,
) -> HostRepositoryResult<()> {
    let message = format!("failed to prepare Git operation: {}", report.error());
    framed
        .write(&GitOpenResponse::Error {
            error: RemoteError::new(ErrorCode::ExecutionFailed, message),
        })
        .await
        .map_err(protocol_report)?;
    Err(report)
}

fn helper_search_path(root: &Path) -> HostRepositoryResult<std::ffi::OsString> {
    let inherited = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .unwrap_or_default();
    std::env::join_paths(std::iter::once(root.to_owned()).chain(inherited)).map_err(|error| {
        invalid_configuration(format!("construct Git remote-helper search path: {error}"))
    })
}

fn bounded_git_error(error: &impl std::fmt::Display) -> String {
    const MAX_CHARS: usize = 2048;

    error.to_string().chars().take(MAX_CHARS).collect()
}

fn repository_approval_update(update: &ReceivedReferenceUpdate) -> RepositoryApprovalUpdate {
    RepositoryApprovalUpdate {
        source: update.source.to_string(),
        destination: update.destination.to_string(),
        expected: update.previous.as_ref().map(ToString::to_string),
        proposed: update.proposed.to_string(),
        allow_rewrite: update.rewrites,
    }
}

fn source_id(source: &str) -> String {
    format!("{:x}", Sha256::digest(source.as_bytes()))
}

fn tracked_refs_digest(refresh: &RepositoryRefresh) -> HostRepositoryResult<String> {
    let mut references = refresh.references.clone();
    references.sort_by(|left, right| left.name.as_str().cmp(right.name.as_str()));
    let mut snapshot = Sha256::new();
    snapshot.update(b"tascarrel.repository-cache.v1\0");
    hash_snapshot_field(
        &mut snapshot,
        refresh
            .default_branch
            .as_ref()
            .map_or(&[], |branch| branch.as_str().as_bytes()),
    )?;
    let reference_count = u64::try_from(references.len())
        .map_err(|_| invalid_request("repository has too many upstream references"))?;
    snapshot.update(reference_count.to_be_bytes());
    for reference in &references {
        hash_repository_reference(&mut snapshot, reference)?;
    }
    Ok(format!("{:x}", snapshot.finalize()))
}

/// Adds one length-delimited value to a repository snapshot digest.
fn hash_snapshot_field(snapshot: &mut Sha256, value: &[u8]) -> HostRepositoryResult<()> {
    let length = u64::try_from(value.len())
        .map_err(|_| invalid_request("repository snapshot field is too large"))?;
    snapshot.update(length.to_be_bytes());
    snapshot.update(value);
    Ok(())
}

/// Adds one complete upstream reference to a repository snapshot digest.
fn hash_repository_reference(
    snapshot: &mut Sha256,
    reference: &RepositoryReference,
) -> HostRepositoryResult<()> {
    snapshot.update([2]);
    hash_snapshot_field(snapshot, reference.name.as_str().as_bytes())?;
    hash_snapshot_field(snapshot, reference.object.as_str().as_bytes())?;
    snapshot.update([match reference.kind {
        ObjectKind::Commit => 1,
        ObjectKind::Tag => 2,
        ObjectKind::Tree => 3,
        ObjectKind::Blob => 4,
    }]);
    hash_snapshot_field(
        snapshot,
        reference
            .peeled_commit
            .as_ref()
            .map_or(&[], |object| object.as_str().as_bytes()),
    )
}

fn validate_path(value: &str) -> HostRepositoryResult<()> {
    let path = Path::new(value);
    if value.is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(invalid_configuration(format!(
            "invalid repository path {value:?}"
        )));
    }
    Ok(())
}

fn validate_source(source: &str) -> HostRepositoryResult<()> {
    if source.is_empty()
        || source.len() > 4096
        || source.chars().any(char::is_control)
        || source.starts_with('-')
    {
        return Err(invalid_configuration("invalid repository source"));
    }
    Ok(())
}

fn validate_branch(branch: &str) -> HostRepositoryResult<()> {
    if branch.is_empty()
        || branch.len() > 1024
        || branch.starts_with('-')
        || branch.starts_with('/')
        || branch.ends_with('/')
        || branch.ends_with('.')
        || branch.contains("..")
        || branch.contains("@{")
        || branch.bytes().any(|byte| {
            byte <= b' '
                || byte == 0x7f
                || matches!(byte, b'~' | b'^' | b':' | b'?' | b'*' | b'[' | b'\\')
        })
    {
        return Err(invalid_request("invalid Git branch"));
    }
    Ok(())
}

fn validate_tag(tag: &str) -> HostRepositoryResult<()> {
    ReferenceName::new(format!("refs/tags/{tag}"))
        .map(|_| ())
        .map_err(git_report)
}

fn display_remote(source: &str) -> String {
    let without_fragment = source.split_once('#').map_or(source, |(value, _)| value);
    let (without_query, had_query) = without_fragment
        .split_once('?')
        .map_or((without_fragment, false), |(value, _)| (value, true));
    let mut display = if let Some((scheme, remainder)) = without_query.split_once("://") {
        let authority_end = remainder.find('/').unwrap_or(remainder.len());
        let (authority, path) = remainder.split_at(authority_end);
        let authority = authority
            .rsplit_once('@')
            .map_or(authority, |(_, host)| host);
        format!("{scheme}://{authority}{path}")
    } else if let Some((_, host_path)) = without_query.split_once('@') {
        host_path.to_owned()
    } else {
        without_query.to_owned()
    };
    if had_query {
        display.push_str("?<redacted>");
    }
    display
}

fn ensure_remote_helper(root: &Path) -> HostRepositoryResult<()> {
    let helper = root.join("git-remote-tascarrel");
    match fs::symlink_metadata(&helper) {
        Ok(metadata) if metadata.file_type().is_symlink() => return Ok(()),
        Ok(_) => {
            return Err(unsafe_state(format!(
                "Git remote helper path is unsafe: {}",
                helper.display()
            )));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(io_report(
                format!("inspect Git remote helper {}", helper.display()),
                error,
            ));
        }
    }
    let executable = std::env::current_exe()
        .map_err(|error| io_report("resolve Tascarrel executable", error))?;
    symlink(executable, &helper).map_err(|error| {
        io_report(
            format!("create Git remote helper {}", helper.display()),
            error,
        )
    })?;
    Ok(())
}

fn create_private_directory(path: &Path) -> HostRepositoryResult<()> {
    if !path.exists() {
        fs::create_dir_all(path).map_err(|error| {
            io_report(
                format!("create repository state root {}", path.display()),
                error,
            )
        })?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|error| {
            io_report(
                format!("set repository state permissions {}", path.display()),
                error,
            )
        })?;
    }
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        io_report(
            format!("inspect repository state root {}", path.display()),
            error,
        )
    })?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(unsafe_state(format!(
            "repository state root is unsafe: {}",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::*;

    /// Verifies repository sources shown to users omit embedded credentials.
    #[test]
    fn repository_inventory_remote_hides_credentials() {
        assert_eq!(
            display_remote("https://user:token@example.com/acme/repo.git?access_token=secret#main"),
            "https://example.com/acme/repo.git?<redacted>"
        );
        assert_eq!(
            display_remote("git@example.com:acme/repo.git"),
            "example.com:acme/repo.git"
        );
        assert_eq!(display_remote("/srv/git/repo.git"), "/srv/git/repo.git");
    }

    /// Verifies workspace deletion cannot remove another workspace's cache.
    #[tokio::test]
    async fn workspace_deletion_removes_only_its_isolated_cache() -> HostRepositoryResult<()> {
        let temporary = test_io(tempfile::tempdir(), "create temporary directory")?;
        let root = temporary.path().join("repos");
        let deleted_cache = root.join("workspaces/demo");
        let retained_cache = root.join("workspaces/other");
        test_io(fs::create_dir_all(&deleted_cache), "create deleted cache")?;
        test_io(fs::create_dir_all(&retained_cache), "create retained cache")?;
        test_io(
            fs::write(deleted_cache.join("cache-marker"), "delete"),
            "write deleted cache marker",
        )?;
        test_io(
            fs::write(retained_cache.join("cache-marker"), "retain"),
            "write retained cache marker",
        )?;

        HostRepositoryManager::remove_workspace_cache(root.clone(), "demo".to_owned()).await?;
        assert!(!deleted_cache.exists());
        assert!(retained_cache.is_dir());
        HostRepositoryManager::remove_workspace_cache(root, "demo".to_owned()).await?;
        Ok(())
    }

    /// Verifies workspace deletion rejects a redirected cache namespace.
    #[tokio::test]
    async fn workspace_deletion_rejects_symlinked_cache_root() -> HostRepositoryResult<()> {
        let temporary = test_io(tempfile::tempdir(), "create temporary directory")?;
        let root = temporary.path().join("repos");
        let outside = temporary.path().join("outside");
        test_io(fs::create_dir_all(&root), "create repository root")?;
        test_io(
            fs::create_dir_all(outside.join("demo")),
            "create external cache",
        )?;
        test_io(
            symlink(&outside, root.join("workspaces")),
            "create redirected cache root",
        )?;

        let error = HostRepositoryManager::remove_workspace_cache(root, "demo".to_owned())
            .await
            .expect_err("symlinked workspace cache root must be rejected");
        assert!(error.to_string().contains("cache root is unsafe"));
        assert!(outside.join("demo").is_dir());
        Ok(())
    }

    /// Verifies an existing manager revalidates changed repository policy.
    #[test]
    fn long_lived_manager_reloads_repository_policy() -> HostRepositoryResult<()> {
        let temporary = test_io(tempfile::tempdir(), "create temporary directory")?;
        let config = temporary.path().join("config.toml");
        test_io(fs::write(&config, ""), "write empty configuration")?;
        let manager = HostRepositoryManager::load(
            test_io(std::env::current_exe(), "resolve test executable")?,
            temporary.path().join("repos"),
            &config,
        )?;
        let source = "https://example.invalid/tascarrel.git";

        assert!(!manager.source_is_currently_configured(source)?);
        test_io(
            fs::write(
                &config,
                format!("[repos.\"src/tascarrel\"]\nsource = {source:?}\n"),
            ),
            "write repository configuration",
        )?;
        assert!(manager.source_is_currently_configured(source)?);
        assert!(manager.source_is_currently_configured_for_path("src/tascarrel", source)?);

        test_io(fs::write(&config, ""), "clear repository configuration")?;
        assert!(!manager.source_is_currently_configured(source)?);
        assert!(!manager.source_is_currently_configured_for_path("src/tascarrel", source)?);
        Ok(())
    }

    /// Verifies a configuration failure is returned as a safe Git handshake
    /// rejection instead of resetting the logical channel.
    #[tokio::test]
    async fn git_handshake_reports_configuration_failure() -> HostRepositoryResult<()> {
        let temporary = test_io(tempfile::tempdir(), "create temporary directory")?;
        let config = temporary.path().join("config.toml");
        let source = "https://example.invalid/tascarrel.git";
        test_io(
            fs::write(
                &config,
                format!("[repos.\"src/tascarrel\"]\nsource = {source:?}\n"),
            ),
            "write repository configuration",
        )?;
        let manager = HostRepositoryManager::load(
            test_io(std::env::current_exe(), "resolve test executable")?,
            temporary.path().join("repos"),
            &config,
        )?;
        test_io(
            fs::write(&config, "[unknown]\nfield = true\n"),
            "write invalid repository configuration",
        )?;

        let (guest_io, host_io) = tokio::io::duplex(64 * 1024);
        let (guest_driver, guest_mux, _guest_incoming) = tascarrel_mux::connect(
            guest_io,
            tascarrel_mux::Role::Client,
            tascarrel_mux::Config::default(),
        )
        .map_err(|error| invalid_request(format!("create guest mux: {error}")))?;
        let (host_driver, _host_mux, mut host_incoming) = tascarrel_mux::connect(
            host_io,
            tascarrel_mux::Role::Server,
            tascarrel_mux::Config::default(),
        )
        .map_err(|error| invalid_request(format!("create host mux: {error}")))?;
        let guest_driver = tokio::spawn(guest_driver.run());
        let host_driver = tokio::spawn(host_driver.run());
        let guest_open = tokio::spawn(async move {
            guest_mux
                .open(tascarrel_protocol::MUX_GIT_HOST_ENDPOINT)
                .await
        });
        let request = host_incoming
            .recv()
            .await
            .ok_or_else(|| invalid_request("host mux closed before Git request"))?;
        let host_channel = request
            .accept()
            .map_err(|error| invalid_request(format!("accept Git channel: {error}")))?;
        let guest_channel = guest_open
            .await
            .map_err(|error| invalid_request(format!("join Git channel open: {error}")))?
            .map_err(|error| invalid_request(format!("open Git channel: {error}")))?;
        let service = tokio::spawn(manager.serve_upload_pack(host_channel));
        let mut guest = Framed::new(guest_channel);
        guest
            .write(&GitHostRequest::ReceivePack {
                source: source.to_owned(),
                pod_id: PodId("pod-test".to_owned()),
                path: "src/tascarrel".to_owned(),
            })
            .await
            .map_err(protocol_report)?;
        let response = guest
            .read::<GitOpenResponse>()
            .await
            .map_err(protocol_report)?
            .ok_or_else(|| invalid_request("host closed Git handshake without a response"))?;
        let GitOpenResponse::Error { error } = response else {
            return Err(invalid_request(
                "host accepted an invalid Git configuration",
            ));
        };
        assert_eq!(error.code, ErrorCode::ExecutionFailed);
        assert_eq!(
            error.message,
            "failed to prepare Git operation: failed to load repository configuration"
        );
        let service_error = service
            .await
            .map_err(|error| invalid_request(format!("join repository service: {error}")))?
            .expect_err("invalid configuration must fail the repository service");
        assert!(matches!(
            service_error.error(),
            HostRepositoryError::InvalidConfiguration
        ));
        guest_driver.abort();
        host_driver.abort();
        Ok(())
    }

    /// Workspace policy applies by default while a repository policy replaces
    /// it as one self-contained override.
    #[test]
    fn repository_policy_overrides_are_complete() -> HostRepositoryResult<()> {
        let temporary = test_io(tempfile::tempdir(), "create temporary directory")?;
        let config = temporary.path().join("config.toml");
        test_io(
            fs::write(
                &config,
                "[git]\ndefault-policy = 'allow'\n\
                 [[git.tags]]\npattern = '**'\npolicy = 'require-approval'\n\
                 [repos.inherited]\nsource = 'https://example.invalid/inherited.git'\n\
                 [repos.overridden]\nsource = 'https://example.invalid/overridden.git'\n\
                 [repos.overridden.git]\ndefault-policy = 'deny'\n\
                 [[repos.overridden.git.branches]]\npattern = 'automation/**'\npolicy = 'allow'\n",
            ),
            "write repository policy configuration",
        )?;

        let parsed = load_workspace_config(&config)?;
        let inherited = &parsed.repos["inherited"].policy;
        let overridden = &parsed.repos["overridden"].policy;
        assert_eq!(
            inherited.reference_policy(&ReferenceName::new("refs/heads/main").map_err(git_report)?),
            RepositoryPushPolicy::Allow
        );
        assert_eq!(
            inherited.reference_policy(&ReferenceName::new("refs/tags/v1").map_err(git_report)?),
            RepositoryPushPolicy::RequireApproval
        );
        assert_eq!(
            overridden.reference_policy(
                &ReferenceName::new("refs/heads/automation/topic").map_err(git_report)?
            ),
            RepositoryPushPolicy::Allow
        );
        assert_eq!(
            overridden
                .reference_policy(&ReferenceName::new("refs/heads/main").map_err(git_report)?),
            RepositoryPushPolicy::Deny
        );
        assert_eq!(
            overridden.reference_policy(&ReferenceName::new("refs/tags/v1").map_err(git_report)?),
            RepositoryPushPolicy::Deny
        );
        Ok(())
    }

    /// Verifies an empty upstream produces a stable successful cache version.
    #[tokio::test]
    async fn cache_versions_accept_an_empty_upstream() -> HostRepositoryResult<()> {
        let temporary = test_io(tempfile::tempdir(), "create temporary directory")?;
        let git = test_git()?;
        let upstream = temporary.path().join("upstream.git");
        run_test_git(&git, temporary.path(), &["init", "--bare", path(&upstream)])?;
        let config = temporary.path().join("config.toml");
        test_io(
            fs::write(
                &config,
                format!(
                    "[repos.\"src/tascarrel\"]\nsource = {:?}\n",
                    path(&upstream)
                ),
            ),
            "write repository configuration",
        )?;
        let manager =
            HostRepositoryManager::load(git, temporary.path().join("repositories"), &config)?;

        let initial = manager.refresh_versions(None).await?;
        assert_eq!(initial.len(), 1);
        assert_eq!(initial[0].version, 1);
        let unchanged = manager.refresh_versions(None).await?;
        assert_eq!(unchanged[0], initial[0]);
        Ok(())
    }

    /// Verifies cache identity remains stable while versions track changed
    /// branches and added or removed tags without advancing when unchanged.
    #[tokio::test]
    async fn cache_versions_track_exact_upstream_refs() -> HostRepositoryResult<()> {
        let temporary = test_io(tempfile::tempdir(), "create temporary directory")?;
        let git = test_git()?;
        let upstream = temporary.path().join("upstream.git");
        let source = temporary.path().join("source");
        run_test_git(&git, temporary.path(), &["init", "--bare", path(&upstream)])?;
        run_test_git(&git, temporary.path(), &["init", path(&source)])?;
        run_test_git(&git, &source, &["config", "user.name", "Tascarrel Test"])?;
        run_test_git(
            &git,
            &source,
            &["config", "user.email", "tascarrel@example.invalid"],
        )?;
        test_io(
            fs::write(source.join("README.md"), "initial\n"),
            "write source",
        )?;
        run_test_git(&git, &source, &["add", "README.md"])?;
        run_test_git(&git, &source, &["commit", "-m", "initial"])?;
        run_test_git(&git, &source, &["branch", "-M", "main"])?;
        run_test_git(&git, &source, &["remote", "add", "origin", path(&upstream)])?;
        run_test_git(&git, &source, &["push", "-u", "origin", "main"])?;
        run_test_git(
            &git,
            &upstream,
            &["symbolic-ref", "HEAD", "refs/heads/main"],
        )?;
        let config = temporary.path().join("config.toml");
        test_io(
            fs::write(
                &config,
                format!(
                    "[repos.\"src/tascarrel\"]\nsource = {:?}\n",
                    path(&upstream)
                ),
            ),
            "write repository configuration",
        )?;
        let manager =
            HostRepositoryManager::load(git, temporary.path().join("repositories"), &config)?;

        let initial = manager.refresh_versions(None).await?;
        assert_eq!(initial.len(), 1);
        assert_eq!(initial[0].version, 1);
        let unchanged = manager.refresh_versions(None).await?;
        assert_eq!(unchanged[0], initial[0]);
        test_io(
            fs::write(source.join("README.md"), "changed\n"),
            "update source",
        )?;
        run_test_git(manager.git.executable(), &source, &["add", "README.md"])?;
        run_test_git(
            manager.git.executable(),
            &source,
            &["commit", "-m", "change"],
        )?;
        run_test_git(
            manager.git.executable(),
            &source,
            &["push", "origin", "main"],
        )?;
        let branch = manager.refresh_versions(None).await?;
        assert_eq!(branch[0].cache_id, initial[0].cache_id);
        assert_eq!(branch[0].version, 2);
        run_test_git(manager.git.executable(), &source, &["tag", "v1"])?;
        run_test_git(
            manager.git.executable(),
            &source,
            &["push", "origin", "refs/tags/v1"],
        )?;
        let tag = manager.refresh_versions(None).await?;
        assert_eq!(tag[0].cache_id, initial[0].cache_id);
        assert_eq!(tag[0].version, 3);
        run_test_git(manager.git.executable(), &source, &["tag", "-d", "v1"])?;
        run_test_git(
            manager.git.executable(),
            &source,
            &["push", "origin", ":refs/tags/v1"],
        )?;
        let removed_tag = manager.refresh_versions(None).await?;
        assert_eq!(removed_tag[0].cache_id, initial[0].cache_id);
        assert_eq!(removed_tag[0].version, 4);
        Ok(())
    }

    fn test_git() -> HostRepositoryResult<PathBuf> {
        let path = std::env::var_os("PATH").ok_or_else(|| invalid_request("PATH is unset"))?;
        std::env::split_paths(&path)
            .map(|directory| directory.join("git"))
            .find(|candidate| candidate.is_file())
            .ok_or_else(|| invalid_request("Git is unavailable for repository tests"))
    }

    fn run_test_git(git: &Path, directory: &Path, arguments: &[&str]) -> HostRepositoryResult<()> {
        let output = Command::new(git)
            .current_dir(directory)
            .args(arguments)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .map_err(|error| io_report("run test Git", error))?;
        if !output.status.success() {
            return Err(invalid_request(format!(
                "test Git failed: {}",
                String::from_utf8_lossy(&output.stderr)
            )));
        }
        Ok(())
    }

    fn path(path: &Path) -> &str {
        path.to_str().expect("test path is UTF-8")
    }

    fn test_io<T>(result: io::Result<T>, action: &'static str) -> HostRepositoryResult<T> {
        result.map_err(|error| io_report(action, error))
    }
}
