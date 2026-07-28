//! Repository inventory, cache invalidation, and detailed change queries.
//!
//! [`ChangesService`] owns the resumable repository-status inventory and
//! coordinates fanotify invalidation with bounded Git refreshes. Detailed
//! queries use exact revisions or an explicitly accepted live working tree.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::num::NonZeroUsize;
use std::os::fd::OwnedFd;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use reportify::ErrorExt as _;
use reportify::Report;
use tascarrel_api::ArcVec;
use tascarrel_api::MAX_RELATIVE_PATH_BYTES;
use tascarrel_api::types::changes as api;
use tascarrel_api::types::files::FileGitStatus;
use tascarrel_api::types::files::FilePath;
use tascarrel_api::types::pods::PodId;
use tascarrel_api::types::store as store_api;
use tascarrel_store::Store;
use thiserror::Error;
use tokio::sync::Mutex;
use tokio::sync::RwLock;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tracing::debug;
use tracing::warn;

use super::git;
use super::git::GitConfig;
use super::git::GitInspectionError;
use super::git::RepositorySnapshot;
use super::watcher::WorkspaceEvent;
use super::watcher::WorkspaceWatcher;
use super::watcher::WorkspaceWatcherError;
use crate::GuestRepositoryManager;
use crate::RepositoryConfigProvider;
use crate::WorkspaceRepository;
use crate::services::pods::PodService;

const DEFAULT_STORE_HISTORY_LIMIT: NonZeroUsize =
    NonZeroUsize::new(1024).expect("default store history limit is nonzero");

/// Live repository inspection service shared by every control-plane connection.
#[derive(Clone)]
pub struct ChangesService {
    inner: Arc<ChangesServiceInner>,
}

impl ChangesService {
    /// Creates an empty changes service using one configured Git executable.
    ///
    /// # Errors
    ///
    /// Returns an internal report unless the Git executable is an absolute
    /// regular file and all configured limits are nonzero.
    pub fn new(config: ChangesServiceConfig) -> Result<Self, Report<ChangesServiceError>> {
        config.validate()?;
        let git = GitConfig {
            executable: config.git.clone(),
            command_timeout: config.command_timeout,
            metadata_bytes: config.metadata_bytes,
            result_bytes: config.result_bytes,
            diagnostic_bytes: config.diagnostic_bytes,
        };
        let store = Store::new(
            api::RepositoryStatusList {
                repositories: ArcVec::new(),
            },
            reduce_repository_status_list,
            config.store_history_limit,
        );
        Ok(Self {
            inner: Arc::new(ChangesServiceInner {
                git,
                config,
                repositories: Mutex::new(BTreeMap::new()),
                watchers: Mutex::new(BTreeMap::new()),
                tracker: Mutex::new(None),
                store,
            }),
        })
    }

    /// Starts pod and repository tracking from operation-provided services.
    ///
    /// Initial inventory discovery completes before this method returns.
    /// Subsequent Git inspection stays in the tracker so ordinary directory
    /// reads do not wait for refreshes.
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn ensure_tracking(
        &self,
        pods: PodService,
        repositories: Option<Arc<GuestRepositoryManager>>,
        repository_config: Option<Arc<dyn RepositoryConfigProvider>>,
    ) {
        let mut tracker = self.inner.tracker.lock().await;
        if tracker.as_ref().is_some_and(|task| !task.is_finished()) {
            return;
        }
        if let Some(task) = tracker.take() {
            task.abort();
        }
        let (initialized_tx, initialized_rx) = oneshot::channel();
        let service = self.clone();
        *tracker = Some(tokio::spawn(async move {
            service
                .track_inventory(pods, repositories, repository_config, initialized_tx)
                .await;
        }));
        drop(tracker);
        if let Err(error) = initialized_rx.await {
            warn!(%error, "repository tracker stopped before initialization completed");
        }
    }

    /// Opens the resumable workspace-wide repository status list.
    ///
    /// # Errors
    ///
    /// Returns a contract report when the cursor generation is invalid.
    pub(crate) fn subscribe(
        &self,
        input: &api::RepositoryStatusListChangedSubscription,
    ) -> Result<RepositoryStatusListSubscription, Report<ChangesServiceError>> {
        let cursor = input.cursor.as_ref().map(runtime_stamp).transpose()?;
        Ok(self.inner.store.subscribe(cursor))
    }

    /// Returns cached Git status overlays for the direct files in one
    /// directory.
    pub(crate) async fn directory_statuses(
        &self,
        pod_id: &PodId,
        directory: &str,
    ) -> BTreeMap<String, FileGitStatus> {
        let mut repositories = self
            .inner
            .repositories
            .lock()
            .await
            .values()
            .filter(|repository| repository.target.pod_id == *pod_id)
            .cloned()
            .collect::<Vec<_>>();
        repositories.sort_by_key(|repository| repository.target.path.as_str().len());
        let mut statuses = BTreeMap::new();
        for repository in repositories {
            let Some(relative) = repository_relative(repository.target.path.as_str(), directory)
            else {
                continue;
            };
            let prefix = (!relative.is_empty()).then(|| format!("{relative}/"));
            let snapshot = repository.snapshot.read().await;
            let Some(snapshot) = snapshot.as_ref() else {
                continue;
            };
            for (path, status) in &snapshot.files {
                let direct = match prefix.as_deref() {
                    Some(prefix) => path.strip_prefix(prefix),
                    None => Some(path.as_str()),
                };
                let Some(name) = direct.filter(|path| !path.contains('/')) else {
                    continue;
                };
                statuses.insert(name.to_owned(), status.clone());
            }
        }
        statuses
    }

    /// Adds a structure watch for one directory visited by `FilesService`.
    pub(crate) async fn watch_directory(&self, pod_id: &PodId, workspace: &Path, relative: &str) {
        let watcher = self
            .inner
            .watchers
            .lock()
            .await
            .get(pod_id)
            .map(|watch| Arc::clone(&watch.watcher));
        let Some(watcher) = watcher else {
            return;
        };
        let workspace = workspace.to_owned();
        let relative = relative.to_owned();
        let result =
            tokio::task::spawn_blocking(move || watcher.watch_directory(&workspace, &relative))
                .await;
        match result {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                warn!(pod_id = %pod_id.0, %error, "could not watch workspace directory");
            }
            Err(error) => {
                warn!(pod_id = %pod_id.0, %error, "workspace directory watch task failed");
            }
        }
    }

    /// Gets complete commits unique to both sides of an exact comparison.
    ///
    /// # Errors
    ///
    /// Returns a contract report for an unknown repository and an unavailable
    /// report when its worktree is not currently accessible.
    #[tracing::instrument(
        level = "debug",
        skip_all,
        fields(pod_id = %input.target.pod_id.0, path = %input.target.path)
    )]
    pub(crate) async fn divergent_commits(
        &self,
        input: api::GetDivergentCommitsAction,
    ) -> Result<api::GetDivergentCommitsOutput, Report<ChangesServiceError>> {
        let repository = self.repository(&input.target).await?;
        let root = repository
            .root
            .read()
            .await
            .clone()
            .ok_or_else(|| unavailable("repository worktree is not currently available"))?;
        let result = match git::divergent_commits(&self.inner.git, &root, &input.comparison).await {
            Ok(commits) => {
                let result = api::DivergentCommitsResult::Commits(commits);
                if encoded_size(&result)? > self.inner.config.result_bytes {
                    too_large_divergence(self.inner.config.result_bytes)
                } else {
                    result
                }
            }
            Err(report) => match report.error() {
                GitInspectionError::RevisionUnavailable(revision) => {
                    api::DivergentCommitsResult::RevisionUnavailable(api::UnavailableGitRevision {
                        revision: revision.clone(),
                    })
                }
                GitInspectionError::TooLarge => {
                    too_large_divergence(self.inner.config.result_bytes)
                }
                GitInspectionError::UnrelatedHistories => {
                    return Err(report.escalate(ChangesServiceError::Internal(
                        "divergence query unexpectedly required a merge base".to_owned(),
                    )));
                }
                GitInspectionError::Failed(message) => {
                    let message = message.clone();
                    return Err(report.escalate(ChangesServiceError::Internal(message)));
                }
            },
        };
        Ok(api::GetDivergentCommitsOutput { result })
    }

    /// Gets one complete current or commit-based change set.
    ///
    /// # Errors
    ///
    /// Returns a contract report for an unknown repository or invalid path and
    /// an unavailable report when its worktree is inaccessible.
    #[tracing::instrument(
        level = "debug",
        skip_all,
        fields(pod_id = %input.target.pod_id.0, path = %input.target.path)
    )]
    pub(crate) async fn change_set(
        &self,
        input: api::GetChangeSetAction,
    ) -> Result<api::GetChangeSetOutput, Report<ChangesServiceError>> {
        let repository = self.repository(&input.target).await?;
        let root = repository
            .root
            .read()
            .await
            .clone()
            .ok_or_else(|| unavailable("repository worktree is not currently available"))?;
        let result = match git::change_set(
            &self.inner.git,
            &root,
            &input.comparison,
            input.path.as_ref(),
        )
        .await
        {
            Ok(change_set) => {
                let result = api::ChangeSetResult::ChangeSet(change_set);
                if encoded_size(&result)? > self.inner.config.result_bytes {
                    too_large_change_set(self.inner.config.result_bytes)
                } else {
                    result
                }
            }
            Err(report) => match report.error() {
                GitInspectionError::RevisionUnavailable(revision) => {
                    api::ChangeSetResult::RevisionUnavailable(api::UnavailableGitRevision {
                        revision: revision.clone(),
                    })
                }
                GitInspectionError::UnrelatedHistories => {
                    let api::ChangeSetComparison::Commits(comparison) = input.comparison else {
                        return Err(report.escalate(ChangesServiceError::Internal(
                            "working change set unexpectedly required a merge base".to_owned(),
                        )));
                    };
                    api::ChangeSetResult::UnrelatedHistories(api::UnrelatedRepositoryHistories {
                        base: comparison.base,
                        head: comparison.head,
                    })
                }
                GitInspectionError::TooLarge => {
                    too_large_change_set(self.inner.config.result_bytes)
                }
                GitInspectionError::Failed(message) => {
                    let message = message.clone();
                    return Err(report.escalate(ChangesServiceError::Internal(message)));
                }
            },
        };
        Ok(api::GetChangeSetOutput { result })
    }

    async fn track_inventory(
        self,
        pods: PodService,
        repositories: Option<Arc<GuestRepositoryManager>>,
        repository_config: Option<Arc<dyn RepositoryConfigProvider>>,
        initialized: oneshot::Sender<()>,
    ) {
        let subscription = pods
            .subscribe(&tascarrel_api::types::pods::PodListChangedSubscription { cursor: None });
        let Ok(mut subscription) = subscription else {
            warn!("could not subscribe to pod inventory for repository tracking");
            notify_tracker_initialized(initialized);
            return;
        };
        if subscription.recv().await.is_none() {
            notify_tracker_initialized(initialized);
            return;
        }
        if let Err(error) = self
            .reconcile_inventory(&pods, repositories.as_deref(), repository_config.as_deref())
            .await
        {
            warn!(%error, "could not reconcile initial repository inventory");
        }
        notify_tracker_initialized(initialized);
        let mut safety = tokio::time::interval(self.inner.config.safety_rescan_interval);
        safety.tick().await;
        loop {
            tokio::select! {
                event = subscription.recv() => {
                    if event.is_none() {
                        return;
                    }
                    if let Err(error) = self.reconcile_inventory(
                        &pods,
                        repositories.as_deref(),
                        repository_config.as_deref(),
                    ).await {
                        warn!(%error, "could not reconcile repository inventory after pod change");
                    }
                }
                _ = safety.tick() => {
                    self.invalidate_all().await;
                    if let Err(error) = self.reconcile_inventory(
                        &pods,
                        repositories.as_deref(),
                        repository_config.as_deref(),
                    ).await {
                        warn!(%error, "could not perform repository safety rescan");
                    }
                }
            }
        }
    }

    async fn reconcile_inventory(
        &self,
        pods: &PodService,
        repositories: Option<&GuestRepositoryManager>,
        repository_config: Option<&dyn RepositoryConfigProvider>,
    ) -> Result<(), Report<ChangesServiceError>> {
        let configured = repository_declarations(repositories, repository_config).await?;
        let configured_paths = configured.keys().cloned().collect::<Vec<_>>();
        for path in &configured_paths {
            validate_repository_path(path)?;
        }
        let public_pods = pods.pod_snapshot();
        let mut desired = BTreeMap::<api::RepositoryTarget, Option<PathBuf>>::new();
        let mut workspace_watches = BTreeMap::<PodId, (PathBuf, Arc<OwnedFd>)>::new();
        if !configured_paths.is_empty() {
            for pod in public_pods {
                let workspace = pods.workspace_root(&pod.id).await;
                match &workspace {
                    Ok(workspace) => match pods.active_workspace_watch(&pod.id) {
                        Ok(Some(watch_mount)) => {
                            workspace_watches
                                .insert(pod.id.clone(), (workspace.clone(), watch_mount));
                        }
                        Ok(None) => {}
                        Err(error) => {
                            warn!(pod_id = %pod.id.0, %error, "failed to resolve active pod workspace mount");
                        }
                    },
                    Err(error) => {
                        warn!(pod_id = %pod.id.0, %error, "failed to resolve pod workspace for repository tracking");
                    }
                }
                for path in &configured_paths {
                    let root = match &workspace {
                        Ok(workspace) => Some(workspace.join(path)),
                        Err(_) => None,
                    };
                    desired.insert(
                        api::RepositoryTarget {
                            pod_id: pod.id.clone(),
                            path: FilePath::new(path.as_str()),
                        },
                        root,
                    );
                }
            }
        }

        let mut repositories = self.inner.repositories.lock().await;
        let removed = repositories
            .keys()
            .filter(|target| !desired.contains_key(*target))
            .cloned()
            .collect::<Vec<_>>();
        for target in &removed {
            repositories.remove(target);
        }
        let mut refresh = Vec::new();
        for (target, root) in desired {
            let repository = repositories
                .entry(target.clone())
                .or_insert_with(|| {
                    Arc::new(RepositoryCache {
                        target,
                        root: RwLock::new(None),
                        snapshot: RwLock::new(None),
                        generation: AtomicU64::new(0),
                        refresh: Mutex::new(()),
                    })
                })
                .clone();
            let root_changed = *repository.root.read().await != root;
            if root_changed {
                *repository.root.write().await = root;
                let mut snapshot = repository.snapshot.write().await;
                repository.generation.fetch_add(1, Ordering::Relaxed);
                *snapshot = None;
            }
            if repository.snapshot.read().await.is_none() {
                refresh.push(repository);
            }
        }
        drop(repositories);
        for target in removed {
            self.inner
                .store
                .apply(api::RepositoryStatusListMutation::Remove(target));
        }

        let desired_pods = workspace_watches.keys().cloned().collect::<BTreeSet<_>>();
        self.remove_stale_watchers(&desired_pods).await;
        for (pod_id, (workspace, watch_mount)) in workspace_watches {
            self.ensure_watcher(pod_id, workspace, watch_mount).await;
        }
        for repository in refresh {
            self.refresh_repository(repository).await;
        }
        Ok(())
    }

    async fn remove_stale_watchers(&self, desired_pods: &BTreeSet<PodId>) {
        let stale_watchers = self
            .inner
            .watchers
            .lock()
            .await
            .keys()
            .filter(|pod_id| !desired_pods.contains(*pod_id))
            .cloned()
            .collect::<Vec<_>>();
        for pod_id in stale_watchers {
            if let Some(watcher) = self.inner.watchers.lock().await.remove(&pod_id) {
                watcher.task.abort();
            }
        }
    }

    async fn ensure_watcher(&self, pod_id: PodId, workspace: PathBuf, watch_mount: Arc<OwnedFd>) {
        let mut watchers = self.inner.watchers.lock().await;
        if watchers.get(&pod_id).is_some_and(|watch| {
            !watch.task.is_finished()
                && watch.workspace == workspace
                && watch.watcher.watches_mount(&watch_mount)
        }) {
            return;
        }
        if let Some(watch) = watchers.remove(&pod_id) {
            watch.task.abort();
        }
        let watcher = match WorkspaceWatcher::new(
            &workspace,
            watch_mount,
            self.inner.config.event_debounce,
            self.inner.config.max_event_batch,
        ) {
            Ok(watcher) => Arc::new(watcher),
            Err(error) => {
                warn!(
                    pod_id = %pod_id.0,
                    workspace = %workspace.display(),
                    %error,
                    "workspace fanotify unavailable"
                );
                return;
            }
        };
        let service = self.clone();
        let input = Arc::clone(&watcher);
        let watched_pod = pod_id.clone();
        let task = tokio::spawn(async move {
            if let Err(error) = service.watch_workspace(watched_pod.clone(), input).await {
                warn!(pod_id = %watched_pod.0, %error, "workspace fanotify stopped");
            }
        });
        watchers.insert(
            pod_id,
            PodWatch {
                workspace,
                watcher,
                task,
            },
        );
    }

    async fn watch_workspace(
        &self,
        pod_id: PodId,
        watcher: Arc<WorkspaceWatcher>,
    ) -> Result<(), Report<WorkspaceWatcherError>> {
        loop {
            let first = watcher.next_batch().await?;
            if first.is_empty() {
                continue;
            }
            self.invalidate_events(&pod_id, &first).await;
            self.refresh_invalidated(&pod_id).await;

            let maximum = tokio::time::Instant::now() + self.inner.config.refresh_max_delay;
            let mut quiet = tokio::time::Instant::now() + self.inner.config.refresh_quiet_period;
            let mut pending = false;
            loop {
                let deadline = quiet.min(maximum);
                tokio::select! {
                    batch = watcher.next_batch() => {
                        let batch = batch?;
                        if batch.is_empty() {
                            continue;
                        }
                        self.invalidate_events(&pod_id, &batch).await;
                        pending = true;
                        quiet = tokio::time::Instant::now() + self.inner.config.refresh_quiet_period;
                        if tokio::time::Instant::now() >= maximum {
                            self.refresh_invalidated(&pod_id).await;
                            break;
                        }
                    }
                    () = tokio::time::sleep_until(deadline) => {
                        if pending {
                            self.refresh_invalidated(&pod_id).await;
                        }
                        break;
                    }
                }
            }
        }
    }

    async fn invalidate_events(&self, pod_id: &PodId, events: &[WorkspaceEvent]) {
        let rescan = events
            .iter()
            .any(|event| event.overflow || event.path.is_none());
        let paths = events
            .iter()
            .filter_map(|event| event.path.as_deref())
            .collect::<Vec<_>>();
        let repositories = self
            .inner
            .repositories
            .lock()
            .await
            .values()
            .filter(|repository| {
                repository.target.pod_id == *pod_id
                    && (rescan
                        || paths
                            .iter()
                            .any(|path| paths_overlap(path, repository.target.path.as_str())))
            })
            .cloned()
            .collect::<Vec<_>>();
        for repository in repositories {
            let mut snapshot = repository.snapshot.write().await;
            repository.generation.fetch_add(1, Ordering::Relaxed);
            *snapshot = None;
        }
    }

    async fn refresh_invalidated(&self, pod_id: &PodId) {
        let repositories = self
            .inner
            .repositories
            .lock()
            .await
            .values()
            .filter(|repository| repository.target.pod_id == *pod_id)
            .cloned()
            .collect::<Vec<_>>();
        for repository in repositories {
            if repository.snapshot.read().await.is_none() {
                self.refresh_repository(repository).await;
            }
        }
    }

    async fn invalidate_all(&self) {
        let repositories = self
            .inner
            .repositories
            .lock()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for repository in repositories {
            let mut snapshot = repository.snapshot.write().await;
            repository.generation.fetch_add(1, Ordering::Relaxed);
            *snapshot = None;
        }
    }

    #[tracing::instrument(
        level = "debug",
        skip_all,
        fields(
            pod_id = %repository.target.pod_id.0,
            path = %repository.target.path
        )
    )]
    async fn refresh_repository(&self, repository: Arc<RepositoryCache>) {
        let _refresh = repository.refresh.lock().await;
        if repository.snapshot.read().await.is_some() {
            return;
        }
        let generation = repository.generation.load(Ordering::Relaxed);
        let root = repository.root.read().await.clone();
        let (snapshot, state) = match root {
            Some(root) => {
                match git::inspect(&self.inner.git, &root, repository.target.path.as_str()).await {
                    Ok(snapshot) => {
                        let status = snapshot.status.clone();
                        (Some(snapshot), api::RepositoryStatusState::Ready(status))
                    }
                    Err(report) => (
                        None,
                        api::RepositoryStatusState::Failed(api::RepositoryInspectionFailure {
                            message: bounded_message(
                                report.to_string(),
                                self.inner.config.status_message_bytes,
                            )
                            .into(),
                        }),
                    ),
                }
            }
            None => (
                None,
                api::RepositoryStatusState::Failed(api::RepositoryInspectionFailure {
                    message: "pod workspace is not currently available".into(),
                }),
            ),
        };
        let mut cached = repository.snapshot.write().await;
        if repository.generation.load(Ordering::Relaxed) != generation {
            return;
        }
        *cached = snapshot;
        drop(cached);
        self.publish(api::RepositoryStatusEntry {
            target: repository.target.clone(),
            state,
        });
    }

    fn publish(&self, entry: api::RepositoryStatusEntry) {
        self.inner
            .store
            .apply(api::RepositoryStatusListMutation::Upsert(entry));
    }

    async fn repository(
        &self,
        target: &api::RepositoryTarget,
    ) -> Result<Arc<RepositoryCache>, Report<ChangesServiceError>> {
        self.inner
            .repositories
            .lock()
            .await
            .get(target)
            .cloned()
            .ok_or_else(|| invalid("repository target is not configured"))
    }
}

/// Runtime and resource limits for repository inspection.
#[derive(Clone, Debug)]
pub struct ChangesServiceConfig {
    /// Absolute Git executable used for local, non-networked inspection.
    pub git: PathBuf,
    /// Maximum duration of one Git subprocess.
    pub command_timeout: Duration,
    /// Maximum bytes captured from metadata-producing Git commands.
    pub metadata_bytes: usize,
    /// Maximum encoded bytes returned by one detailed action.
    pub result_bytes: usize,
    /// Maximum diagnostic bytes retained from Git standard error.
    pub diagnostic_bytes: usize,
    /// Maximum diagnostic bytes published in repository status.
    pub status_message_bytes: usize,
    /// Maximum interval used to coalesce one fanotify event batch.
    pub event_debounce: Duration,
    /// Maximum fanotify events retained in one batch before forcing a rescan.
    pub max_event_batch: usize,
    /// Quiet interval before the trailing refresh of an event burst.
    pub refresh_quiet_period: Duration,
    /// Maximum delay before refreshing during a continuous event burst.
    pub refresh_max_delay: Duration,
    /// Periodic fallback rescan interval for missed filesystem events.
    pub safety_rescan_interval: Duration,
    /// Number of repository-list mutations retained for subscription
    /// resumption.
    pub store_history_limit: NonZeroUsize,
}

impl ChangesServiceConfig {
    /// Creates repository inspection defaults for one Git executable.
    #[must_use]
    pub fn new(git: impl Into<PathBuf>) -> Self {
        Self {
            git: git.into(),
            command_timeout: Duration::from_secs(30),
            metadata_bytes: 16 * 1024 * 1024,
            result_bytes: 6 * 1024 * 1024,
            diagnostic_bytes: 64 * 1024,
            status_message_bytes: 4 * 1024,
            event_debounce: Duration::from_millis(120),
            max_event_batch: 512,
            refresh_quiet_period: Duration::from_millis(350),
            refresh_max_delay: Duration::from_secs(2),
            safety_rescan_interval: Duration::from_mins(5),
            store_history_limit: DEFAULT_STORE_HISTORY_LIMIT,
        }
    }

    fn validate(&self) -> Result<(), Report<ChangesServiceError>> {
        if !self.git.is_absolute() || !self.git.is_file() {
            return Err(internal("Git executable must be an absolute regular file"));
        }
        if self.command_timeout.is_zero()
            || self.metadata_bytes == 0
            || self.result_bytes == 0
            || self.diagnostic_bytes == 0
            || self.status_message_bytes == 0
            || self.event_debounce.is_zero()
            || self.max_event_batch == 0
            || self.refresh_quiet_period.is_zero()
            || self.refresh_max_delay < self.refresh_quiet_period
            || self.safety_rescan_interval.is_zero()
        {
            return Err(internal(
                "changes service limits must be nonzero and ordered",
            ));
        }
        Ok(())
    }
}

/// Failure from repository inspection orchestration.
#[derive(Debug, Error)]
pub enum ChangesServiceError {
    /// The request violates a repository inspection contract.
    #[error("invalid changes request: {0}")]
    InvalidRequest(String),
    /// The selected pod repository is not currently available.
    #[error("repository changes are unavailable: {0}")]
    Unavailable(String),
    /// Repository inspection failed unexpectedly.
    #[error("changes service failed: {0}")]
    Internal(String),
}

struct ChangesServiceInner {
    git: GitConfig,
    config: ChangesServiceConfig,
    repositories: Mutex<BTreeMap<api::RepositoryTarget, Arc<RepositoryCache>>>,
    watchers: Mutex<BTreeMap<PodId, PodWatch>>,
    tracker: Mutex<Option<JoinHandle<()>>>,
    store: RepositoryStatusStore,
}

struct RepositoryCache {
    target: api::RepositoryTarget,
    root: RwLock<Option<PathBuf>>,
    snapshot: RwLock<Option<RepositorySnapshot>>,
    generation: AtomicU64,
    refresh: Mutex<()>,
}

struct PodWatch {
    workspace: PathBuf,
    watcher: Arc<WorkspaceWatcher>,
    task: JoinHandle<()>,
}

type RepositoryStatusStore = Store<api::RepositoryStatusList, api::RepositoryStatusListMutation>;
pub(crate) type RepositoryStatusListSubscription =
    tascarrel_store::Subscription<api::RepositoryStatusList, api::RepositoryStatusListMutation>;

/// Reads current repository declarations without coupling service construction.
async fn repository_declarations(
    repositories: Option<&GuestRepositoryManager>,
    repository_config: Option<&dyn RepositoryConfigProvider>,
) -> Result<BTreeMap<String, WorkspaceRepository>, Report<ChangesServiceError>> {
    match repositories {
        Some(repositories) => repositories
            .capture_repositories(repository_config)
            .await
            .map(|config| config.repositories)
            .map_err(|error| internal(format!("failed to read repository configuration: {error}"))),
        None => match repository_config {
            Some(repository_config) => repository_config
                .repository_config()
                .await
                .map(|config| config.repositories)
                .map_err(|error| {
                    internal(format!("failed to read repository configuration: {error}"))
                }),
            None => Ok(BTreeMap::new()),
        },
    }
}

fn reduce_repository_status_list(
    list: &mut api::RepositoryStatusList,
    mutation: &api::RepositoryStatusListMutation,
) {
    match mutation {
        api::RepositoryStatusListMutation::Upsert(entry) => {
            if let Some(index) = list
                .repositories
                .iter()
                .position(|current| current.target == entry.target)
            {
                list.repositories[index] = entry.clone();
            } else {
                list.repositories.push(entry.clone());
            }
            list.repositories
                .sort_by(|left, right| left.target.cmp(&right.target));
        }
        api::RepositoryStatusListMutation::Remove(target) => {
            if let Some(index) = list
                .repositories
                .iter()
                .position(|entry| entry.target == *target)
            {
                list.repositories.remove(index);
            }
        }
    }
}

/// Releases callers waiting for the repository tracker's initial inventory.
fn notify_tracker_initialized(initialized: oneshot::Sender<()>) {
    if initialized.send(()).is_err() {
        debug!("repository tracker initialization receiver was dropped");
    }
}

fn runtime_stamp(
    stamp: &store_api::Stamp,
) -> Result<tascarrel_store::Stamp, Report<ChangesServiceError>> {
    let generation = stamp.generation.parse::<uuid::Uuid>().map_err(|error| {
        ChangesServiceError::InvalidRequest("repository-status cursor generation is invalid".into())
            .report()
            .message(error.to_string())
    })?;
    Ok(tascarrel_store::Stamp {
        generation,
        version: stamp.version,
    })
}

fn encoded_size(value: &impl serde::Serialize) -> Result<usize, Report<ChangesServiceError>> {
    serde_json::to_vec(value)
        .map(|encoded| encoded.len())
        .map_err(|error| internal(format!("failed to encode detailed changes result: {error}")))
}

fn too_large_divergence(maximum: usize) -> api::DivergentCommitsResult {
    api::DivergentCommitsResult::TooLarge(api::ResultTooLarge {
        maximum_bytes: maximum as u64,
    })
}

fn too_large_change_set(maximum: usize) -> api::ChangeSetResult {
    api::ChangeSetResult::TooLarge(api::ResultTooLarge {
        maximum_bytes: maximum as u64,
    })
}

fn validate_repository_path(path: &str) -> Result<(), Report<ChangesServiceError>> {
    if path.is_empty()
        || path.len() > MAX_RELATIVE_PATH_BYTES
        || path.starts_with('/')
        || path.ends_with('/')
        || path
            .split('/')
            .any(|part| part.is_empty() || matches!(part, "." | ".."))
    {
        Err(internal(
            "configured repository path is not normalized and relative",
        ))
    } else {
        Ok(())
    }
}

fn path_contains(repository: &str, path: &str) -> bool {
    path == repository
        || path
            .strip_prefix(repository)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn paths_overlap(path: &str, repository: &str) -> bool {
    path_contains(repository, path) || path_contains(path, repository)
}

fn repository_relative<'a>(repository: &str, path: &'a str) -> Option<&'a str> {
    if path == repository {
        Some("")
    } else {
        path.strip_prefix(repository)?.strip_prefix('/')
    }
}

fn bounded_message(mut message: String, maximum_bytes: usize) -> String {
    const ELLIPSIS: &str = "…";

    if message.len() <= maximum_bytes {
        return message;
    }
    let suffix = (maximum_bytes >= ELLIPSIS.len()).then_some(ELLIPSIS);
    let mut boundary = maximum_bytes - suffix.map_or(0, str::len);
    while !message.is_char_boundary(boundary) {
        boundary -= 1;
    }
    message.truncate(boundary);
    if let Some(suffix) = suffix {
        message.push_str(suffix);
    }
    message
}

fn invalid(message: impl Into<String>) -> Report<ChangesServiceError> {
    ChangesServiceError::InvalidRequest(message.into()).report()
}

fn unavailable(message: impl Into<String>) -> Report<ChangesServiceError> {
    ChangesServiceError::Unavailable(message.into()).report()
}

fn internal(message: impl Into<String>) -> Report<ChangesServiceError> {
    ChangesServiceError::Internal(message.into()).report()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Treats repository ancestors and descendants as overlapping invalidation
    /// paths.
    #[test]
    fn repository_overlap_is_bidirectional() {
        assert!(paths_overlap("repo/src/lib.rs", "repo"));
        assert!(paths_overlap("repo", "repo/nested"));
        assert!(!paths_overlap("repository", "repo"));
    }

    /// Keeps the repository status list in stable composite-key order.
    #[test]
    fn repository_reducer_sorts_entries() {
        let mut list = api::RepositoryStatusList {
            repositories: ArcVec::new(),
        };
        let state = api::RepositoryStatusState::Failed(api::RepositoryInspectionFailure {
            message: "fixture".into(),
        });
        for path in ["z", "a"] {
            reduce_repository_status_list(
                &mut list,
                &api::RepositoryStatusListMutation::Upsert(api::RepositoryStatusEntry {
                    target: api::RepositoryTarget {
                        pod_id: tascarrel_api::ids::PodId::generate(),
                        path: FilePath::new(path),
                    },
                    state: state.clone(),
                }),
            );
        }
        assert!(list.repositories[0].target < list.repositories[1].target);
    }

    /// Keeps published diagnostics within their byte bound without splitting
    /// UTF-8 code points.
    #[test]
    fn bounded_diagnostic_preserves_utf8_boundaries() {
        let message = "ééé".to_owned();
        assert_eq!(bounded_message(message.clone(), 4), "…");
        assert_eq!(bounded_message(message, 2), "é");
    }
}
