//! Repository reconciliation and guest Git transport.
//!
//! [`GuestRepositoryManager`] updates golden and image-owned workspace seeds,
//! and resolves pod execution identities for authenticated Git operations.

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::fs::{self};
use std::io::Write as _;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt as _;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::fs::symlink;
use std::path::Path;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use futures_util::StreamExt as _;
use futures_util::TryStreamExt as _;
use jiff::Timestamp;
use reportify::ErrorExt as _;
use reportify::Report;
use reportify::ResultExt as _;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest as _;
use sha2::Sha256;
use tascarrel_api::ids::RepositoryCacheId;
use tascarrel_api::types::repositories::RepositoryCacheVersion;
use tascarrel_protocol::ErrorCode;
use tascarrel_protocol::Framed;
use tascarrel_protocol::GitHostRequest;
use tascarrel_protocol::GitOpenResponse;
use tascarrel_protocol::MUX_GIT_HOST_ENDPOINT;
use tascarrel_protocol::PodGitRequest;
use tascarrel_protocol::PodGitService;
use tascarrel_protocol::RemoteError;
use thiserror::Error;
use tokio::io::AsyncReadExt;
use tokio::net::UnixListener;
use tokio::process::Command;
use tokio::sync::Mutex;
use tokio::sync::RwLock;
use tracing::warn;

use crate::GuestNetworkService;
use crate::RepositoryStorage;
use crate::WorkspaceRepository;
use crate::runtime::pod::BtrfsStore;
use crate::runtime::pod::ImageConfig;
use crate::runtime::pod::ImageId;
use crate::services::pods::PodExecution;
use crate::services::pods::PodService;

const DEFAULT_RECONCILIATION_CONCURRENCY: usize = 4;
const CHECKOUT_MARKER_SCHEMA: u32 = 1;
const CHECKOUT_SCHEMA: u32 = 1;
const CHECKOUT_MARKER_FILE: &str = "tascarrel-cache.json";
const MAX_CHECKOUT_MARKER_BYTES: u64 = 64 * 1024;

/// Failure while serving a Git operation from an authenticated pod channel.
#[derive(Debug, Error)]
pub enum PodGitError {
    /// A framed request or response could not be exchanged.
    #[error("pod Git protocol failed")]
    Protocol,
    /// The host-owned Git endpoint could not be opened.
    #[error("failed to open the host Git transport")]
    HostTransport,
    /// Git protocol bytes could not be relayed between the pod and host.
    #[error("failed to relay pod Git data")]
    Relay,
}

/// Supplies the current repository declarations for a managed workspace.
#[async_trait::async_trait]
pub trait RepositoryConfigProvider: Send + Sync {
    /// Refreshes workspace input and returns its configured repositories.
    async fn repositories(&self) -> Result<BTreeMap<String, WorkspaceRepository>, RemoteError>;
}

/// Reconciles the golden workspace and image-owned workspace seeds.
pub struct GuestRepositoryManager {
    repositories: RwLock<BTreeMap<String, WorkspaceRepository>>,
    store: Arc<BtrfsStore>,
    network_service: Arc<GuestNetworkService>,
    root: PathBuf,
    runtime: PathBuf,
    git: PathBuf,
    btrfs: PathBuf,
    cp: PathBuf,
    overlay: Option<PathBuf>,
    reconciliation: Mutex<()>,
}

/// Host-refreshed repository state supplied to an image service operation.
#[derive(Clone)]
pub(crate) struct RepositoryPreparation {
    manager: Arc<GuestRepositoryManager>,
    repositories: Arc<BTreeMap<String, WorkspaceRepository>>,
    versions: Arc<BTreeMap<String, RepositoryCacheVersion>>,
    refreshed_input: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct CheckoutMarker {
    schema_version: u32,
    checkout_schema: u32,
    source_id: String,
    cache_id: RepositoryCacheId,
    cache_version: u64,
    cache_updated_at: Timestamp,
}

impl CheckoutMarker {
    fn new(source: &str, cache: &RepositoryCacheVersion) -> Self {
        Self {
            schema_version: CHECKOUT_MARKER_SCHEMA,
            checkout_schema: CHECKOUT_SCHEMA,
            source_id: source_id(source),
            cache_id: cache.cache_id.clone(),
            cache_version: cache.version,
            cache_updated_at: cache.updated_at,
        }
    }

    fn validate(&self) -> Result<()> {
        if self.schema_version != CHECKOUT_MARKER_SCHEMA
            || self.checkout_schema == 0
            || self.cache_version == 0
            || !is_digest(&self.source_id)
        {
            bail!("managed repository marker is invalid");
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReconciliationAction {
    Clone,
    Fetch,
    Configure,
    Unchanged,
}

struct VersionedReconciliation {
    path: String,
    repository: WorkspaceRepository,
    version: RepositoryCacheVersion,
    existing: Option<CheckoutMarker>,
    action: ReconciliationAction,
}

impl RepositoryPreparation {
    /// Creates operation-scoped repository preparation state.
    pub(crate) fn new_versioned(
        manager: Arc<GuestRepositoryManager>,
        repositories: BTreeMap<String, WorkspaceRepository>,
        versions: impl IntoIterator<Item = RepositoryCacheVersion>,
        refreshed_input: bool,
    ) -> Result<Self, RemoteError> {
        let mut by_path = BTreeMap::new();
        for version in versions {
            if version.version == 0
                || !repositories.contains_key(version.path.as_ref())
                || by_path.insert(version.path.to_string(), version).is_some()
            {
                return Err(RemoteError::new(
                    ErrorCode::ExecutionFailed,
                    "host returned invalid repository cache versions",
                ));
            }
        }
        if by_path.len() != repositories.len() {
            return Err(RemoteError::new(
                ErrorCode::ExecutionFailed,
                "host repository cache versions do not match the workspace configuration",
            ));
        }
        Ok(Self {
            manager,
            repositories: Arc::new(repositories),
            versions: Arc::new(by_path),
            refreshed_input,
        })
    }

    /// Reports whether repository capture already refreshed image inputs for
    /// this operation.
    pub(crate) fn input_was_refreshed(&self) -> bool {
        self.refreshed_input
    }

    /// Reconciles the golden workspace used by a new image setup run.
    pub(crate) async fn reconcile_golden(&self, image: &ImageId) -> Result<(), RemoteError> {
        self.manager
            .reconcile_golden_versioned(image, &self.repositories, &self.versions)
            .await
    }

    /// Updates one image's canonical workspace seed without running setup.
    pub(crate) async fn update_image_seed(&self, image: &ImageId) -> Result<bool, RemoteError> {
        self.manager
            .update_image_seed_versioned(image, &self.repositories, &self.versions)
            .await
    }
}

impl std::fmt::Debug for GuestRepositoryManager {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let repositories = self
            .repositories
            .try_read()
            .map_or(0, |repositories| repositories.len());
        formatter
            .debug_struct("GuestRepositoryManager")
            .field("root", &self.root)
            .field("repositories", &repositories)
            .finish_non_exhaustive()
    }
}

impl GuestRepositoryManager {
    /// Serves one Git operation over the authenticated pod multiplexer.
    ///
    /// # Errors
    ///
    /// Returns a typed error when framing, host transport, or byte relay fails.
    #[tracing::instrument(level = "debug", skip_all, fields(pod_id = %pod_id.0), err)]
    pub async fn serve_pod_git(
        &self,
        channel: tascarrel_mux::Channel,
        pod_id: tascarrel_api::types::pods::PodId,
        pods: &PodService,
    ) -> Result<(), Report<PodGitError>> {
        let mut framed = Framed::new(channel);
        let request = framed
            .read::<PodGitRequest>()
            .await
            .escalate(PodGitError::Protocol)?
            .ok_or_else(|| PodGitError::Protocol.report())?;
        let pod_id = tascarrel_protocol::PodId(pod_id.0.to_string());
        match request.service {
            PodGitService::UploadPack => {
                self.serve_pod_fetch(framed, pod_id, request.path, pods)
                    .await
            }
            PodGitService::ReceivePack => {
                self.serve_pod_push(framed, pod_id, request.path, pods)
                    .await
            }
        }
    }

    /// Relays one authenticated pod upload-pack request to hostd.
    async fn serve_pod_fetch(
        &self,
        mut framed: Framed<tascarrel_mux::Channel>,
        pod_id: tascarrel_protocol::PodId,
        path: String,
        pods: &PodService,
    ) -> Result<(), Report<PodGitError>> {
        let Some(repository) = self.repositories.read().await.get(&path).cloned() else {
            framed
                .write(&GitOpenResponse::Error {
                    error: RemoteError::new(
                        ErrorCode::PermissionDenied,
                        "repository path is not configured",
                    ),
                })
                .await
                .escalate(PodGitError::Protocol)?;
            return Ok(());
        };
        if let Err(error) = Self::pod_account(pods, &pod_id).await {
            framed
                .write(&GitOpenResponse::Error { error })
                .await
                .escalate(PodGitError::Protocol)?;
            return Ok(());
        }
        let channel = self
            .network_service
            .open_channel(MUX_GIT_HOST_ENDPOINT)
            .await
            .map_err(|error| error.escalate(PodGitError::HostTransport))?;
        let mut host = Framed::new(channel);
        host.write(&GitHostRequest::UploadPack {
            source: repository.source,
            refresh: true,
            expected_cache_id: None,
            expected_version: None,
        })
        .await
        .escalate(PodGitError::Protocol)?;
        let response = host
            .read::<GitOpenResponse>()
            .await
            .escalate(PodGitError::Protocol)?
            .unwrap_or_else(|| GitOpenResponse::Error {
                error: RemoteError::new(
                    ErrorCode::ExecutionFailed,
                    "host closed the Git fetch channel before accepting it",
                ),
            });
        framed
            .write(&response)
            .await
            .escalate(PodGitError::Protocol)?;
        if !matches!(response, GitOpenResponse::Ready) {
            return Ok(());
        }
        let mut pod = framed.into_inner();
        let mut host = host.into_inner();
        tokio::io::copy_bidirectional(&mut pod, &mut host)
            .await
            .escalate(PodGitError::Relay)?;
        Ok(())
    }

    /// Relays one authenticated pod receive-pack request to hostd.
    async fn serve_pod_push(
        &self,
        mut framed: Framed<tascarrel_mux::Channel>,
        pod_id: tascarrel_protocol::PodId,
        path: String,
        pods: &PodService,
    ) -> Result<(), Report<PodGitError>> {
        let Some(repository) = self.repositories.read().await.get(&path).cloned() else {
            framed
                .write(&GitOpenResponse::Error {
                    error: RemoteError::new(
                        ErrorCode::PermissionDenied,
                        "repository path is not configured",
                    ),
                })
                .await
                .escalate(PodGitError::Protocol)?;
            return Ok(());
        };
        if let Err(error) = Self::pod_account(pods, &pod_id).await {
            framed
                .write(&GitOpenResponse::Error { error })
                .await
                .escalate(PodGitError::Protocol)?;
            return Ok(());
        }
        let channel = self
            .network_service
            .open_channel(MUX_GIT_HOST_ENDPOINT)
            .await
            .map_err(|error| error.escalate(PodGitError::HostTransport))?;
        let mut host = Framed::new(channel);
        host.write(&GitHostRequest::ReceivePack {
            source: repository.source,
            pod_id,
            path,
        })
        .await
        .escalate(PodGitError::Protocol)?;
        let response = host
            .read::<GitOpenResponse>()
            .await
            .escalate(PodGitError::Protocol)?
            .ok_or_else(|| PodGitError::Protocol.report())?;
        framed
            .write(&response)
            .await
            .escalate(PodGitError::Protocol)?;
        if !matches!(response, GitOpenResponse::ReceivePackReady { .. }) {
            return Ok(());
        }
        let mut pod = framed.into_inner();
        let mut host = host.into_inner();
        tokio::io::copy_bidirectional(&mut pod, &mut host)
            .await
            .escalate(PodGitError::Relay)?;
        Ok(())
    }

    /// Captures current repository declarations for one image operation.
    pub(crate) async fn capture_repositories(
        &self,
        config_provider: Option<&dyn RepositoryConfigProvider>,
    ) -> Result<BTreeMap<String, WorkspaceRepository>, RemoteError> {
        let repositories = if let Some(provider) = config_provider {
            provider.repositories().await?
        } else {
            self.repositories.read().await.clone()
        };
        *self.repositories.write().await = repositories.clone();
        Ok(repositories)
    }

    /// Creates a workspace seed reconciler and its remote-helper directory.
    ///
    /// # Errors
    ///
    /// Returns an error for non-absolute tools or unsafe state paths.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        repositories: BTreeMap<String, WorkspaceRepository>,
        store: Arc<BtrfsStore>,
        network_service: Arc<GuestNetworkService>,
        storage: &RepositoryStorage,
        runtime: PathBuf,
        git: PathBuf,
        btrfs: PathBuf,
        cp: PathBuf,
        overlay: Option<PathBuf>,
    ) -> Result<Arc<Self>> {
        let root = storage.root().to_owned();
        for path in [&root, &runtime, &git, &btrfs, &cp] {
            if !path.is_absolute() {
                bail!(
                    "repository manager path is not absolute: {}",
                    path.display()
                );
            }
        }
        create_directory(&root, 0o755)?;
        create_directory(&root.join("staging"), 0o755)?;
        create_directory(&runtime, 0o755)?;
        store.enable_pod_workspace_traversal()?;
        store.enable_image_seed_traversal()?;
        let helper = runtime.join("git-remote-tascarrel");
        publish_managed_symlink(&helper, &std::env::current_exe()?, "Git remote helper")?;
        Ok(Arc::new(Self {
            repositories: RwLock::new(repositories),
            store,
            network_service,
            root,
            runtime,
            git,
            btrfs,
            cp,
            overlay,
            reconciliation: Mutex::new(()),
        }))
    }

    async fn pod_account(
        pods: &PodService,
        pod_id: &tascarrel_protocol::PodId,
    ) -> Result<PodExecution, RemoteError> {
        let pod_id = tascarrel_api::types::pods::PodId::from_str(&pod_id.0)
            .map_err(|error| RemoteError::new(ErrorCode::InvalidRequest, error.to_string()))?;
        pods.execution(&pod_id)
            .await
            .map_err(|error| RemoteError::new(ErrorCode::ExecutionFailed, error.to_string()))
    }

    /// Reconciles the golden workspace against exact per-cache versions.
    pub(crate) async fn reconcile_golden_versioned(
        &self,
        image: &ImageId,
        repositories: &BTreeMap<String, WorkspaceRepository>,
        versions: &BTreeMap<String, RepositoryCacheVersion>,
    ) -> Result<(), RemoteError> {
        let _operation = self.reconciliation.lock().await;
        let image = self.store.image(image).map_err(remote_error)?;
        let source = self.store.golden_workspace().map_err(remote_error)?;
        let source = source.filter(|source| {
            fs::metadata(source).is_ok_and(|metadata| {
                metadata.uid() == image.config().user().uid()
                    && metadata.gid() == image.config().user().gid()
            })
        });
        let staging = self
            .stage_workspace(source.as_deref(), image.config())
            .await
            .map_err(remote_error)?;
        let result = async {
            self.reconcile_repositories_versioned(
                &staging,
                image.config(),
                repositories,
                versions,
                true,
            )
            .await?;
            self.store
                .publish_golden_workspace(&staging)
                .map_err(anyhow::Error::from)?;
            Ok::<(), anyhow::Error>(())
        }
        .await;
        self.remove_staging(&staging).await;
        result.map_err(remote_error)
    }

    /// Atomically updates one image seed using only repositories whose cache
    /// identity, content version, or checkout schema changed.
    pub(crate) async fn update_image_seed_versioned(
        &self,
        image: &ImageId,
        repositories: &BTreeMap<String, WorkspaceRepository>,
        versions: &BTreeMap<String, RepositoryCacheVersion>,
    ) -> Result<bool, RemoteError> {
        let _operation = self.reconciliation.lock().await;
        *self.repositories.write().await = repositories.clone();
        let image = self.store.image(image).map_err(remote_error)?;
        let Some(source) = self
            .store
            .image_workspace_seed(image.id())
            .map_err(remote_error)?
        else {
            return Err(RemoteError::new(
                ErrorCode::ExecutionFailed,
                "image has no prepared workspace seed",
            ));
        };
        if Self::workspace_matches_versioned(&source, repositories, versions)
            .map_err(remote_error)?
        {
            return Ok(false);
        }
        let staging = self
            .stage_workspace(Some(&source), image.config())
            .await
            .map_err(remote_error)?;
        let result = async {
            self.reconcile_repositories_versioned(
                &staging,
                image.config(),
                repositories,
                versions,
                false,
            )
            .await?;
            self.store
                .publish_image_workspace_seed(image.id(), &staging)
                .map_err(anyhow::Error::from)?;
            Ok::<(), anyhow::Error>(())
        }
        .await;
        self.remove_staging(&staging).await;
        result.map(|()| true).map_err(remote_error)
    }

    async fn reconcile_repositories_versioned(
        &self,
        workspace: &Path,
        image: &ImageConfig,
        repositories: &BTreeMap<String, WorkspaceRepository>,
        versions: &BTreeMap<String, RepositoryCacheVersion>,
        apply_overlay: bool,
    ) -> Result<()> {
        let managed = managed_versioned_repositories(workspace)?;
        for path in managed
            .keys()
            .filter(|path| !repositories.contains_key(path.as_str()))
        {
            remove_managed_checkout(workspace, path)?;
        }

        let mut work = Vec::with_capacity(repositories.len());
        for (path, repository) in repositories {
            let version = versions
                .get(path)
                .ok_or_else(|| anyhow::anyhow!("host cache version is missing for {path}"))?;
            let expected = CheckoutMarker::new(&repository.source, version);
            let checkout = workspace.join(path);
            let existing = managed.get(path);
            let action = if let Some(existing) = existing {
                if existing.source_id != expected.source_id
                    || existing.cache_id != expected.cache_id
                {
                    remove_managed_checkout(workspace, path)?;
                    ReconciliationAction::Clone
                } else if existing.cache_version > expected.cache_version {
                    bail!("host cache version regressed for managed repository {path}");
                } else if existing.cache_version < expected.cache_version {
                    ReconciliationAction::Fetch
                } else if existing.checkout_schema != CHECKOUT_SCHEMA {
                    ReconciliationAction::Configure
                } else {
                    ReconciliationAction::Unchanged
                }
            } else if checkout.exists() {
                bail!("repository destination is not managed by Tascarrel: {path}");
            } else {
                ReconciliationAction::Clone
            };
            if action == ReconciliationAction::Clone {
                let parent = checkout.parent().expect("repository path has a parent");
                create_owned_tree(
                    workspace,
                    parent
                        .strip_prefix(workspace)
                        .expect("checkout parent is below workspace"),
                    image.user().uid(),
                    image.user().gid(),
                )?;
            }
            if action != ReconciliationAction::Unchanged {
                work.push(VersionedReconciliation {
                    path: path.clone(),
                    repository: repository.clone(),
                    version: version.clone(),
                    existing: existing.cloned(),
                    action,
                });
            }
        }

        self.apply_versioned_reconciliation(workspace, image, work)
            .await?;

        if apply_overlay && let Some(overlay) = &self.overlay {
            validate_overlay(overlay)?;
            let source = overlay.join(".");
            let output = Command::new(&self.cp)
                .args(["-R", "--no-preserve=ownership", "--"])
                .arg(source)
                .arg(workspace)
                .uid(image.user().uid())
                .gid(image.user().gid())
                .output()
                .await?;
            success(&output, "copy workspace overlay")?;
        }
        Ok(())
    }

    async fn apply_versioned_reconciliation(
        &self,
        workspace: &Path,
        image: &ImageConfig,
        work: Vec<VersionedReconciliation>,
    ) -> Result<()> {
        futures_util::stream::iter(work.into_iter().map(|work| async move {
            let checkout = workspace.join(&work.path);
            match work.action {
                ReconciliationAction::Clone => {
                    self.clone_repository(
                        &work.repository.source,
                        &checkout,
                        image,
                        Some(&work.version),
                    )
                    .await?;
                    self.configure_repository(&checkout, &work.path, image)
                        .await?;
                }
                ReconciliationAction::Fetch => {
                    self.fetch_repository(
                        &work.repository.source,
                        &checkout,
                        image,
                        Some(&work.version),
                    )
                    .await?;
                    if work
                        .existing
                        .as_ref()
                        .is_some_and(|marker| marker.checkout_schema != CHECKOUT_SCHEMA)
                    {
                        self.configure_repository(&checkout, &work.path, image)
                            .await?;
                    }
                }
                ReconciliationAction::Configure => {
                    self.configure_repository(&checkout, &work.path, image)
                        .await?;
                }
                ReconciliationAction::Unchanged => {
                    unreachable!("unchanged repositories are excluded from reconciliation")
                }
            }
            write_checkout_marker(
                &checkout,
                &CheckoutMarker::new(&work.repository.source, &work.version),
                image.user().uid(),
                image.user().gid(),
            )
        }))
        .buffer_unordered(DEFAULT_RECONCILIATION_CONCURRENCY)
        .try_collect::<Vec<_>>()
        .await?;
        Ok(())
    }

    async fn configure_repository(
        &self,
        checkout: &Path,
        path: &str,
        image: &ImageConfig,
    ) -> Result<()> {
        let origin = format!("tascarrel://workspace/{path}");
        let push_origin = format!("file:///workspace/{path}");
        for (key, value, operation) in [
            (
                "remote.origin.url",
                origin.as_str(),
                "configure brokered repository origin",
            ),
            (
                "remote.origin.pushurl",
                push_origin.as_str(),
                "configure repository push bridge origin",
            ),
            (
                "remote.origin.receivepack",
                "/usr/local/bin/tascarrel-git-receive-pack",
                "configure pod repository push bridge",
            ),
        ] {
            let output = self
                .git_command(checkout, image, ["config", "--local", key, value])
                .output()
                .await?;
            success(&output, operation)?;
        }
        Ok(())
    }

    async fn fetch_repository(
        &self,
        source: &str,
        checkout: &Path,
        image: &ImageConfig,
        cache: Option<&RepositoryCacheVersion>,
    ) -> Result<()> {
        let mut child = self.git_command(
            checkout,
            image,
            [
                "fetch",
                "--force",
                "--prune",
                "--prune-tags",
                "tascarrel://forge",
                "+refs/heads/*:refs/remotes/origin/*",
                "+refs/tags/*:refs/tags/*",
                "+HEAD:refs/remotes/origin/TASCARREL_HEAD",
            ],
        );
        self.run_transport(source, cache, &mut child, "guest Git fetch")
            .await?;
        let output = self
            .git_command(
                checkout,
                image,
                ["reset", "--hard", "refs/remotes/origin/TASCARREL_HEAD"],
            )
            .output()
            .await?;
        success(&output, "reset managed repository to refreshed upstream")
    }

    async fn clone_repository(
        &self,
        source: &str,
        destination: &Path,
        image: &ImageConfig,
        cache: Option<&RepositoryCacheVersion>,
    ) -> Result<()> {
        let mut child = Command::new(&self.git);
        child
            .args(["clone", "--no-hardlinks", "tascarrel://forge"])
            .arg(destination)
            .uid(image.user().uid())
            .gid(image.user().gid())
            .kill_on_drop(true);
        self.run_transport(source, cache, &mut child, "guest Git clone")
            .await
    }

    async fn run_transport(
        &self,
        source: &str,
        cache: Option<&RepositoryCacheVersion>,
        command: &mut Command,
        operation: &'static str,
    ) -> Result<()> {
        let socket = self
            .runtime
            .join(format!("git-{}.sock", uuid::Uuid::new_v4()));
        let listener = UnixListener::bind(&socket)?;
        fs::set_permissions(&socket, fs::Permissions::from_mode(0o666))?;
        command
            .env("TASCARREL_GIT_SOCKET", &socket)
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    self.runtime.display(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            )
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let mut child = command
            .spawn()
            .with_context(|| format!("start {operation}"))?;
        let (mut helper, _) = tokio::select! {
            accepted = listener.accept() => accepted?,
            status = child.wait() => {
                let status = status?;
                let diagnostic = child_diagnostic(&mut child).await;
                bail!("{operation} exited before opening its transport: {status}: {diagnostic}");
            }
        };
        let channel = self
            .network_service
            .open_channel(MUX_GIT_HOST_ENDPOINT)
            .await
            .map_err(anyhow::Error::from)?;
        let mut framed = Framed::new(channel);
        framed
            .write(&GitHostRequest::UploadPack {
                source: source.to_owned(),
                refresh: false,
                expected_cache_id: cache.map(|cache| cache.cache_id.0.to_string()),
                expected_version: cache.map(|cache| cache.version),
            })
            .await?;
        match framed.read::<GitOpenResponse>().await? {
            Some(GitOpenResponse::Ready) => {}
            Some(GitOpenResponse::ReceivePackReady { .. }) => {
                bail!("host returned a receive-pack response for Git clone")
            }
            Some(GitOpenResponse::Error { error }) => return Err(error.into()),
            None => bail!("host closed Git channel before accepting it"),
        }
        let mut channel = framed.into_inner();
        tokio::io::copy_bidirectional(&mut helper, &mut channel).await?;
        drop(helper);
        drop(channel);
        let status = child.wait().await?;
        let diagnostic = child_diagnostic(&mut child).await;
        if let Err(error) = fs::remove_file(&socket) {
            warn!(path = %socket.display(), %error, "could not remove repository helper socket");
        }
        if !status.success() {
            bail!("{operation} exited with {status}: {diagnostic}");
        }
        Ok(())
    }

    fn git_command<const N: usize>(
        &self,
        checkout: &Path,
        image: &ImageConfig,
        arguments: [&str; N],
    ) -> Command {
        let mut command = Command::new(&self.git);
        command
            .arg("-C")
            .arg(checkout)
            .args(arguments)
            .uid(image.user().uid())
            .gid(image.user().gid());
        command
    }

    fn workspace_matches_versioned(
        workspace: &Path,
        repositories: &BTreeMap<String, WorkspaceRepository>,
        versions: &BTreeMap<String, RepositoryCacheVersion>,
    ) -> Result<bool> {
        let managed = managed_versioned_repositories(workspace)?;
        if managed.len() != repositories.len() || versions.len() != repositories.len() {
            return Ok(false);
        }
        for (path, repository) in repositories {
            let Some(version) = versions.get(path) else {
                return Ok(false);
            };
            if managed.get(path) != Some(&CheckoutMarker::new(&repository.source, version)) {
                return Ok(false);
            }
        }
        Ok(true)
    }

    async fn stage_workspace(&self, source: Option<&Path>, image: &ImageConfig) -> Result<PathBuf> {
        let staging = self
            .root
            .join("staging")
            .join(uuid::Uuid::new_v4().to_string());
        let mut command = Command::new(&self.btrfs);
        command.args(["subvolume", "snapshot"]);
        if let Some(source) = source {
            command.arg(source).arg(&staging);
        } else {
            command = Command::new(&self.btrfs);
            command.args(["subvolume", "create"]).arg(&staging);
        }
        let output = command.output().await?;
        success(
            &output,
            "create repository reconciliation staging workspace",
        )?;
        fs::set_permissions(&staging, fs::Permissions::from_mode(0o755))?;
        set_owner(&staging, image.user().uid(), image.user().gid())?;
        Ok(staging)
    }

    async fn remove_staging(&self, staging: &Path) {
        if let Err(error) = self
            .btrfs(&["subvolume", "delete", "--recursive"], staging)
            .await
        {
            warn!(path = %staging.display(), %error, "could not remove repository reconciliation staging workspace");
        }
    }

    async fn btrfs(&self, arguments: &[&str], path: &Path) -> Result<()> {
        let output = Command::new(&self.btrfs)
            .args(arguments)
            .arg(path)
            .output()
            .await?;
        success(&output, "manage repository checkout subvolume")
    }
}

fn managed_versioned_repositories(workspace: &Path) -> Result<BTreeMap<String, CheckoutMarker>> {
    let mut managed = BTreeMap::new();
    let mut pending = vec![workspace.to_path_buf()];
    while let Some(directory) = pending.pop() {
        if directory != workspace {
            let git_directory = directory.join(".git");
            match fs::symlink_metadata(&git_directory) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    bail!(
                        "managed repository Git directory is unsafe: {}",
                        git_directory.display()
                    );
                }
                Ok(metadata) if metadata.is_dir() => {
                    if let Some(marker) = read_checkout_marker(&directory)? {
                        let relative = directory
                            .strip_prefix(workspace)
                            .expect("discovered checkout is below workspace")
                            .to_str()
                            .ok_or_else(|| anyhow::anyhow!("repository path is not UTF-8"))?
                            .to_owned();
                        managed.insert(relative, marker);
                    }
                    continue;
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        for entry in fs::read_dir(&directory)? {
            let entry = entry?;
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path)?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                continue;
            }
            if path.file_name().is_some_and(|name| name == ".git") {
                continue;
            }
            pending.push(path);
        }
    }
    Ok(managed)
}

fn read_checkout_marker(checkout: &Path) -> Result<Option<CheckoutMarker>> {
    let marker_path = checkout.join(".git").join(CHECKOUT_MARKER_FILE);
    let metadata = match fs::symlink_metadata(&marker_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.len() > MAX_CHECKOUT_MARKER_BYTES
    {
        bail!(
            "managed repository marker is unsafe: {}",
            marker_path.display()
        );
    }
    let marker: CheckoutMarker = serde_json::from_slice(&fs::read(&marker_path)?)?;
    marker.validate()?;
    Ok(Some(marker))
}

fn write_checkout_marker(
    checkout: &Path,
    marker: &CheckoutMarker,
    uid: u32,
    gid: u32,
) -> Result<()> {
    marker.validate()?;
    let git_directory = checkout.join(".git");
    let metadata = fs::symlink_metadata(&git_directory)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!(
            "managed repository Git directory is unsafe: {}",
            git_directory.display()
        );
    }
    let final_path = git_directory.join(CHECKOUT_MARKER_FILE);
    let temporary = git_directory.join(format!(
        ".{CHECKOUT_MARKER_FILE}.{}.tmp",
        uuid::Uuid::new_v4()
    ));
    let bytes = serde_json::to_vec(marker)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_CHECKOUT_MARKER_BYTES {
        bail!("managed repository marker is too large");
    }
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o644)
        .open(&temporary)?;
    let result = (|| {
        file.write_all(&bytes)?;
        file.sync_all()?;
        set_owner(&temporary, uid, gid)?;
        fs::rename(&temporary, &final_path)?;
        std::fs::File::open(&git_directory)?.sync_all()?;
        Ok::<(), anyhow::Error>(())
    })();
    if result.is_err()
        && let Err(error) = fs::remove_file(&temporary)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        warn!(path = %temporary.display(), %error, "could not remove temporary repository marker");
    }
    result
}

fn validate_overlay(root: &Path) -> Result<()> {
    const MAX_ENTRIES: usize = 100_000;
    const MAX_BYTES: u64 = 1024 * 1024 * 1024;

    let metadata = fs::symlink_metadata(root)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!(
            "workspace overlay is not a real directory: {}",
            root.display()
        );
    }
    let mut pending = vec![root.to_path_buf()];
    let mut entries = 0_usize;
    let mut bytes = 0_u64;
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(&directory)? {
            let entry = entry?;
            let metadata = fs::symlink_metadata(entry.path())?;
            entries = entries
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("workspace overlay entry count overflowed"))?;
            if entries > MAX_ENTRIES {
                bail!("workspace overlay has more than {MAX_ENTRIES} entries");
            }
            if metadata.is_dir() {
                pending.push(entry.path());
            } else if metadata.is_file() {
                if metadata.nlink() != 1 {
                    bail!("workspace overlay contains a hard-linked file");
                }
                bytes = bytes
                    .checked_add(metadata.len())
                    .ok_or_else(|| anyhow::anyhow!("workspace overlay size overflowed"))?;
                if bytes > MAX_BYTES {
                    bail!("workspace overlay exceeds {MAX_BYTES} bytes");
                }
            } else if !metadata.file_type().is_symlink() {
                bail!("workspace overlay contains a special file");
            }
        }
    }
    Ok(())
}

async fn child_diagnostic(child: &mut tokio::process::Child) -> String {
    const LIMIT: u64 = 8 * 1024;
    let Some(stderr) = child.stderr.as_mut() else {
        return String::new();
    };
    let mut bytes = Vec::new();
    if let Err(error) = stderr.take(LIMIT).read_to_end(&mut bytes).await {
        warn!(%error, "could not read repository helper diagnostic");
    }
    String::from_utf8_lossy(&bytes).trim().to_owned()
}

fn success(output: &std::process::Output, operation: &str) -> Result<()> {
    if output.status.success() {
        Ok(())
    } else {
        bail!(
            "{operation} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )
    }
}

fn source_id(source: &str) -> String {
    format!("{:x}", Sha256::digest(source.as_bytes()))
}

fn is_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn remove_managed_checkout(workspace: &Path, relative: &str) -> Result<()> {
    let checkout = workspace.join(relative);
    let metadata = fs::symlink_metadata(&checkout)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("managed repository path is unsafe: {}", checkout.display());
    }
    let canonical_workspace = fs::canonicalize(workspace)?;
    let canonical_checkout = fs::canonicalize(&checkout)?;
    if canonical_checkout == canonical_workspace
        || !canonical_checkout.starts_with(&canonical_workspace)
    {
        bail!(
            "managed repository escapes its workspace: {}",
            checkout.display()
        );
    }
    fs::remove_dir_all(&checkout)?;
    remove_empty_checkout_parents(workspace, &checkout)?;
    Ok(())
}

/// Removes now-empty checkout ancestors without removing the workspace root.
fn remove_empty_checkout_parents(workspace: &Path, checkout: &Path) -> Result<()> {
    let mut parent = checkout.parent();
    while let Some(directory) = parent
        && directory != workspace
    {
        match fs::remove_dir(directory) {
            Ok(()) => parent = directory.parent(),
            Err(error) if error.kind() == std::io::ErrorKind::DirectoryNotEmpty => break,
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn remote_error(error: impl std::fmt::Display) -> RemoteError {
    RemoteError::new(ErrorCode::ExecutionFailed, error.to_string())
}

fn create_directory(path: &Path, mode: u32) -> Result<()> {
    if !path.exists() {
        fs::create_dir_all(path)?;
    }
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        bail!("directory path is unsafe: {}", path.display());
    }
    fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    Ok(())
}

fn set_owner(path: &Path, uid: u32, gid: u32) -> Result<()> {
    nix::unistd::chown(
        path,
        Some(nix::unistd::Uid::from_raw(uid)),
        Some(nix::unistd::Gid::from_raw(gid)),
    )?;
    Ok(())
}

fn create_owned_tree(root: &Path, relative: &Path, uid: u32, gid: u32) -> Result<()> {
    let mut current = root.to_path_buf();
    for component in relative.components() {
        current.push(component);
        if !current.exists() {
            fs::create_dir(&current)?;
            fs::set_permissions(&current, fs::Permissions::from_mode(0o755))?;
            set_owner(&current, uid, gid)?;
        }
    }
    Ok(())
}

/// Atomically refreshes a managed link without following its old target.
fn publish_managed_symlink(path: &Path, target: &Path, purpose: &'static str) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("managed {purpose} has no parent: {}", path.display()))?;
    let temporary = parent.join(format!(".managed-link-{}", uuid::Uuid::new_v4()));
    symlink(target, &temporary)
        .with_context(|| format!("create {purpose} {}", temporary.display()))?;
    if let Err(error) = fs::rename(&temporary, path) {
        if let Err(cleanup_error) = fs::remove_file(&temporary) {
            warn!(
                path = %temporary.display(),
                %cleanup_error,
                "could not remove unpublished managed link"
            );
        }
        return Err(error).with_context(|| format!("publish {purpose} {}", path.display()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verifies an existing repository state directory is repaired to the
    /// requested access mode.
    #[test]
    fn create_directory_repairs_existing_permissions() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let root = temporary.path().join("repo-checkouts");

        create_directory(&root, 0o711)?;
        assert_eq!(fs::metadata(&root)?.permissions().mode() & 0o777, 0o711);
        create_directory(&root, 0o755)?;
        assert_eq!(fs::metadata(&root)?.permissions().mode() & 0o777, 0o755);
        Ok(())
    }

    /// Replaces a broken helper link during a guest-image upgrade.
    #[test]
    fn executable_symlink_publication_replaces_broken_links() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let helper = temporary.path().join("tascarrel-git-receive-pack");
        symlink("/nix/store/removed-tascarrel-guest", &helper)?;

        publish_managed_symlink(
            &helper,
            Path::new("/nix/store/current-tascarrel-guest"),
            "test helper",
        )?;

        assert_eq!(
            fs::read_link(helper)?,
            Path::new("/nix/store/current-tascarrel-guest")
        );
        Ok(())
    }

    /// Versioned checkout markers provide a Git-free equality check and reject
    /// redirected metadata paths.
    #[test]
    fn checkout_markers_track_exact_cache_versions_safely() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let workspace = temporary.path().join("workspace");
        let checkout = workspace.join("src/tascarrel");
        fs::create_dir_all(checkout.join(".git"))?;
        let version = RepositoryCacheVersion {
            path: "src/tascarrel".into(),
            cache_id: RepositoryCacheId::generate(),
            version: 7,
            updated_at: Timestamp::now(),
        };
        let marker = CheckoutMarker::new("https://example.invalid/tascarrel.git", &version);
        write_checkout_marker(
            &checkout,
            &marker,
            nix::unistd::getuid().as_raw(),
            nix::unistd::getgid().as_raw(),
        )?;

        let managed = managed_versioned_repositories(&workspace)?;
        assert_eq!(managed.get("src/tascarrel"), Some(&marker));
        let mut advanced = version.clone();
        advanced.version += 1;
        assert_ne!(
            managed.get("src/tascarrel"),
            Some(&CheckoutMarker::new(
                "https://example.invalid/tascarrel.git",
                &advanced,
            ))
        );

        fs::remove_file(checkout.join(".git").join(CHECKOUT_MARKER_FILE))?;
        symlink(
            "/etc/passwd",
            checkout.join(".git").join(CHECKOUT_MARKER_FILE),
        )?;
        assert!(managed_versioned_repositories(&workspace).is_err());
        fs::remove_file(checkout.join(".git").join(CHECKOUT_MARKER_FILE))?;
        fs::remove_dir(checkout.join(".git"))?;
        symlink("/tmp", checkout.join(".git"))?;
        assert!(managed_versioned_repositories(&workspace).is_err());
        Ok(())
    }

    /// Parent directories remain until their final managed checkout is removed.
    #[test]
    fn managed_checkout_removal_prunes_empty_parents() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let workspace = temporary.path().join("workspace");
        fs::create_dir_all(workspace.join("src/first/.git"))?;
        fs::create_dir_all(workspace.join("src/second/.git"))?;

        remove_managed_checkout(&workspace, "src/first")?;
        assert!(workspace.join("src").is_dir());

        remove_managed_checkout(&workspace, "src/second")?;
        assert!(!workspace.join("src").exists());
        assert!(workspace.is_dir());
        Ok(())
    }
}
