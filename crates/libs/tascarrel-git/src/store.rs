//! Managed bare object stores, hidden captures, approval review, and upstream
//! publication.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs;
use std::fs::OpenOptions;
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use reportify::Report;
use tokio::process::Command;
use tokio::sync::Mutex;

use crate::ApprovalId;
use crate::CaptureId;
use crate::CapturedReference;
use crate::GitBinary;
use crate::GitCommit;
use crate::GitError;
use crate::GitLimits;
use crate::GitResult;
use crate::GitSignature;
use crate::ObjectId;
use crate::ObjectKind;
use crate::PodId;
use crate::PublishOutcome;
use crate::PublishedReference;
use crate::ReceiveNamespace;
use crate::ReceivedReferenceUpdate;
use crate::RefUpdate;
use crate::ReferenceComparison;
use crate::ReferenceName;
use crate::Remote;
use crate::RepositoryReference;
use crate::RepositoryRefresh;
use crate::RepositoryStatistics;
use crate::SourceReference;
use crate::WorkspaceId;
use crate::command::GitCommandOutput;

const TASCARREL_REFS: &str = "refs/tascarrel";
const APPROVAL_REFS: &str = "refs/tascarrel/approvals";
/// Hidden dangling target used to prevent stale cached `HEAD` advertisements.
const NO_DEFAULT_BRANCH_REF: &str = "refs/tascarrel/no-default-branch";
const WORKSPACE_REFS: &str = "refs/tascarrel/workspaces";
const RECEIVE_UPDATE_HOOK: &[u8] = b"#!/bin/sh\ncase \"$1\" in\nrefs/heads/*|refs/tags/*) exit 0 ;;\n*) echo 'Tascarrel accepts pushes only to branches and tags' >&2; exit 1 ;;\nesac\n";

/// One managed bare Git object store.
///
/// A store retains upstream branches and tags alongside hidden Tascarrel
/// capture refs. The object database is reusable by host-side transport caches
/// and guest-side repository services; callers retain authority over repository
/// identity, credentials, approval, and persistence policy.
#[derive(Clone, Debug)]
pub struct RepositoryStore {
    core: Arc<RepositoryStoreCore>,
}

impl RepositoryStore {
    /// Opens or initializes a managed bare repository with default limits.
    ///
    /// # Errors
    ///
    /// Returns a report when the path is unsafe, Git initialization fails, or
    /// an existing directory is not a usable bare repository.
    pub async fn open(git: GitBinary, path: impl Into<PathBuf>) -> GitResult<Self> {
        Self::open_with_limits(git, path, GitLimits::default()).await
    }

    /// Opens an existing managed bare repository without changing it.
    ///
    /// This is intended for inventory and diagnostic surfaces which must not
    /// initialize a cache, rewrite configuration, or change permissions.
    ///
    /// # Errors
    ///
    /// Returns a report when the path is unsafe or does not contain a usable
    /// bare Git repository.
    pub fn open_existing(git: GitBinary, path: impl Into<PathBuf>) -> GitResult<Self> {
        let path = path.into();
        validate_repository_directory(&path)?;
        Ok(Self::from_parts(git, path, GitLimits::default()))
    }

    /// Opens or initializes a managed bare repository with explicit limits.
    ///
    /// # Errors
    ///
    /// Returns a report when a limit is zero, the path is unsafe, Git
    /// initialization fails, or an existing directory is not a usable bare
    /// repository.
    pub async fn open_with_limits(
        git: GitBinary,
        path: impl Into<PathBuf>,
        limits: GitLimits,
    ) -> GitResult<Self> {
        let path = path.into();
        Self::open_path(git, path, limits).await
    }

    #[tracing::instrument(
        name = "tascarrel_git.store.open",
        level = "info",
        skip(git, limits),
        fields(repository = %path.display()),
        err
    )]
    async fn open_path(git: GitBinary, path: PathBuf, limits: GitLimits) -> GitResult<Self> {
        validate_limits(&limits)?;
        if !path.is_absolute() {
            return Err(Report::new(GitError::InvalidRepositoryPath { path }));
        }
        let exists = match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    return Err(Report::new(GitError::InvalidRepository { path }));
                }
                true
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(source) => {
                return Err(Report::new(GitError::Io {
                    action: "inspect the managed Git repository",
                    source,
                }));
            }
        };
        if !exists {
            let mut command = git.command();
            command.args(["init", "--bare", "--quiet", "--"]).arg(&path);
            git.run(command, "initialize a bare repository", &limits, &[])
                .await?
                .success("initialize a bare repository")?;
        }
        validate_bare_repository(&path)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).map_err(|source| {
            Report::new(GitError::Io {
                action: "set managed Git repository permissions",
                source,
            })
        })?;
        let hooks = path.join("tascarrel-hooks");
        fs::create_dir_all(&hooks).map_err(|source| {
            Report::new(GitError::Io {
                action: "create the managed Git hooks directory",
                source,
            })
        })?;
        fs::set_permissions(&hooks, fs::Permissions::from_mode(0o700)).map_err(|source| {
            Report::new(GitError::Io {
                action: "set managed Git hooks directory permissions",
                source,
            })
        })?;
        install_receive_update_hook(&path, &hooks)?;

        let store = Self::from_parts(git, path, limits);
        for (key, value) in [
            ("maintenance.auto", "false"),
            ("transfer.hideRefs", TASCARREL_REFS),
            ("uploadpack.hideRefs", "refs/namespaces"),
            ("uploadpack.allowAnySHA1InWant", "false"),
            ("uploadpack.allowReachableSHA1InWant", "false"),
            ("uploadpack.allowTipSHA1InWant", "false"),
        ] {
            store.configure(key, value).await?;
        }
        store
            .configure("core.hooksPath", hooks.to_string_lossy().as_ref())
            .await?;
        Ok(store)
    }

    fn from_parts(git: GitBinary, path: PathBuf, limits: GitLimits) -> Self {
        Self {
            core: Arc::new(RepositoryStoreCore {
                git,
                path,
                limits,
                mutation: Mutex::new(()),
            }),
        }
    }

    /// Returns the absolute bare repository path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.core.path
    }

    /// Returns the configured Git executable.
    #[must_use]
    pub fn git(&self) -> &GitBinary {
        &self.core.git
    }

    /// Refreshes upstream branches and tags using the configured Git process.
    ///
    /// Hidden Tascarrel refs are unaffected. Upstream credentials and transport
    /// configuration are resolved entirely by the Git process environment.
    ///
    /// # Errors
    ///
    /// Returns a report when the remote cannot be fetched or its refs cannot
    /// be parsed within the configured bounds.
    #[tracing::instrument(
        name = "tascarrel_git.store.refresh",
        level = "info",
        skip(self, remote),
        fields(repository = %self.path().display()),
        err
    )]
    pub async fn refresh(&self, remote: &Remote) -> GitResult<Vec<RepositoryReference>> {
        Ok(self.refresh_snapshot(remote).await?.references)
    }

    /// Refreshes upstream branches and tags and returns the complete observed
    /// state without querying the upstream default branch twice.
    ///
    /// # Errors
    ///
    /// Returns a report when the remote cannot be fetched or its refs cannot
    /// be parsed within the configured bounds.
    #[tracing::instrument(
        name = "tascarrel_git.store.refresh_snapshot",
        level = "info",
        skip(self, remote),
        fields(repository = %self.path().display()),
        err
    )]
    pub async fn refresh_snapshot(&self, remote: &Remote) -> GitResult<RepositoryRefresh> {
        let _mutation = self.core.mutation.lock().await;
        let default_branch = self.refresh_locked(remote).await?;
        let references = self.references_locked().await?;
        Ok(RepositoryRefresh {
            default_branch,
            references,
        })
    }

    /// Lists retained upstream branches and tags without contacting a remote.
    ///
    /// # Errors
    ///
    /// Returns a report when Git fails or produces malformed or excessive
    /// structured output.
    #[tracing::instrument(
        name = "tascarrel_git.store.references",
        level = "debug",
        skip(self),
        fields(repository = %self.path().display()),
        err
    )]
    pub async fn references(&self) -> GitResult<Vec<RepositoryReference>> {
        self.references_locked().await
    }

    /// Resolves the branch selected by the remote's symbolic `HEAD`.
    ///
    /// Returns `None` when the remote does not advertise a branch symbolic
    /// `HEAD`, including when its selected branch is unborn.
    ///
    /// # Errors
    ///
    /// Returns a report when the remote cannot be queried or its advertised
    /// symbolic `HEAD` cannot be parsed.
    #[tracing::instrument(
        name = "tascarrel_git.store.default_branch",
        level = "debug",
        skip(self, remote),
        fields(repository = %self.path().display()),
        err
    )]
    pub async fn default_branch(&self, remote: &Remote) -> GitResult<Option<ReferenceName>> {
        self.remote_default_branch(remote).await
    }

    /// Returns the branch advertised as `HEAD` by this cached store.
    ///
    /// # Errors
    ///
    /// Returns a report when Git fails or the cached symbolic `HEAD` is
    /// malformed.
    #[tracing::instrument(
        name = "tascarrel_git.store.cached_default_branch",
        level = "debug",
        skip(self),
        fields(repository = %self.path().display()),
        err
    )]
    pub async fn cached_default_branch(&self) -> GitResult<Option<ReferenceName>> {
        let mut command = self.command();
        command.args(["symbolic-ref", "--quiet", "HEAD"]);
        let output = self
            .run(command, "resolve the cached default branch", &[])
            .await?;
        match output.status.code() {
            Some(0) => {}
            Some(1) => return Ok(None),
            _ => return Err(output.failure("resolve the cached default branch")),
        }
        let value = output.stdout_text("resolve the cached default branch")?;
        let reference = ReferenceName::new(value.trim())?;
        if !reference.is_branch() || self.rev_parse(reference.as_str()).await?.is_none() {
            return Ok(None);
        }
        Ok(Some(reference))
    }

    /// Imports one exact source ref into a capture-specific hidden ref.
    ///
    /// Git negotiates against the existing object database, so repeated
    /// captures transfer only objects missing from this store.
    ///
    /// # Errors
    ///
    /// Returns a report when the source cannot be fetched, the hidden ref is
    /// invalid, or the resulting object cannot be inspected.
    #[tracing::instrument(
        name = "tascarrel_git.store.import_capture",
        level = "info",
        skip(self, remote),
        fields(
            repository = %self.path().display(),
            workspace_id = %workspace_id,
            pod_id = %pod_id,
            capture_id = %capture_id,
            source = %source,
        ),
        err
    )]
    pub async fn import_capture(
        &self,
        remote: &Remote,
        workspace_id: &WorkspaceId,
        pod_id: &PodId,
        source: &SourceReference,
        capture_id: &CaptureId,
    ) -> GitResult<CapturedReference> {
        let retained_as = capture_reference(workspace_id, pod_id, capture_id, source)?;
        let _mutation = self.core.mutation.lock().await;
        if let Some(object) = self.rev_parse(retained_as.as_str()).await? {
            let kind = self.object_kind(&object).await?;
            let peeled_commit = self.peel_commit(&retained_as).await?;
            return Ok(CapturedReference {
                workspace_id: workspace_id.clone(),
                pod_id: pod_id.clone(),
                capture_id: capture_id.clone(),
                source: source.clone(),
                retained_as,
                object,
                kind,
                peeled_commit,
            });
        }
        let mut command = self.command();
        command.args([
            "fetch",
            "--quiet",
            "--atomic",
            "--no-auto-gc",
            "--no-tags",
            "--no-write-fetch-head",
            "--",
        ]);
        command
            .arg(remote.as_str())
            .arg(format!("{}:{}", source.as_str(), retained_as.as_str()));
        self.run(command, "import a captured ref", &[remote.as_str()])
            .await?
            .success("import a captured ref")?;
        let inspected = self.inspect_reference(&retained_as).await?;
        Ok(CapturedReference {
            workspace_id: workspace_id.clone(),
            pod_id: pod_id.clone(),
            capture_id: capture_id.clone(),
            source: source.clone(),
            retained_as,
            object: inspected.object,
            kind: inspected.kind,
            peeled_commit: inspected.peeled_commit,
        })
    }

    /// Removes every hidden ref belonging to one capture.
    ///
    /// Objects become eligible for later maintenance only when no other refs
    /// retain them.
    ///
    /// # Errors
    ///
    /// Returns a report when refs cannot be enumerated or deleted.
    #[tracing::instrument(
        name = "tascarrel_git.store.remove_capture",
        level = "info",
        skip(self),
        fields(
            repository = %self.path().display(),
            workspace_id = %workspace_id,
            pod_id = %pod_id,
            capture_id = %capture_id,
        ),
        err
    )]
    pub async fn remove_capture(
        &self,
        workspace_id: &WorkspaceId,
        pod_id: &PodId,
        capture_id: &CaptureId,
    ) -> GitResult<usize> {
        let _mutation = self.core.mutation.lock().await;
        let prefix =
            format!("{WORKSPACE_REFS}/{workspace_id}/pods/{pod_id}/captures/{capture_id}/");
        let refs = self.reference_names(&[&prefix]).await?;
        self.delete_references(&refs).await?;
        Ok(refs.len())
    }

    /// Removes every hidden capture ref belonging to one pod.
    ///
    /// Objects become eligible for later maintenance only when no other refs
    /// retain them.
    ///
    /// # Errors
    ///
    /// Returns a report when refs cannot be enumerated or deleted.
    #[tracing::instrument(
        name = "tascarrel_git.store.remove_pod",
        level = "info",
        skip(self),
        fields(
            repository = %self.path().display(),
            workspace_id = %workspace_id,
            pod_id = %pod_id,
        ),
        err
    )]
    pub async fn remove_pod(&self, workspace_id: &WorkspaceId, pod_id: &PodId) -> GitResult<usize> {
        let _mutation = self.core.mutation.lock().await;
        let prefix = format!("{WORKSPACE_REFS}/{workspace_id}/pods/{pod_id}/");
        let refs = self.reference_names(&[&prefix]).await?;
        self.delete_references(&refs).await?;
        Ok(refs.len())
    }

    /// Removes every hidden capture ref belonging to one workspace.
    ///
    /// # Errors
    ///
    /// Returns a report when refs cannot be enumerated or deleted.
    #[tracing::instrument(
        name = "tascarrel_git.store.remove_workspace",
        level = "info",
        skip(self),
        fields(repository = %self.path().display(), workspace_id = %workspace_id),
        err
    )]
    pub async fn remove_workspace(&self, workspace_id: &WorkspaceId) -> GitResult<usize> {
        let _mutation = self.core.mutation.lock().await;
        let prefix = format!("{WORKSPACE_REFS}/{workspace_id}/");
        let refs = self.reference_names(&[&prefix]).await?;
        self.delete_references(&refs).await?;
        Ok(refs.len())
    }

    /// Retains one approval baseline object under a hidden review ref.
    ///
    /// # Errors
    ///
    /// Returns a report when the object is unavailable or Git cannot update
    /// the hidden ref.
    #[tracing::instrument(
        name = "tascarrel_git.store.retain_approval_base",
        level = "info",
        skip(self, object),
        fields(
            repository = %self.path().display(),
            approval_id = %approval_id,
            update = update_index,
        ),
        err
    )]
    pub async fn retain_approval_base(
        &self,
        approval_id: &ApprovalId,
        update_index: usize,
        object: &ObjectId,
    ) -> GitResult<ReferenceName> {
        let _mutation = self.core.mutation.lock().await;
        let reference = approval_base_reference(approval_id, update_index)?;
        let mut command = self.command();
        command
            .args(["update-ref", "--create-reflog", "--"])
            .arg(reference.as_str())
            .arg(object.as_str());
        self.run(command, "retain repository approval base", &[])
            .await?
            .success("retain repository approval base")?;
        Ok(reference)
    }

    /// Removes every hidden baseline ref retained for one approval.
    ///
    /// # Errors
    ///
    /// Returns a report when refs cannot be enumerated or deleted.
    #[tracing::instrument(
        name = "tascarrel_git.store.remove_approval",
        level = "info",
        skip(self),
        fields(repository = %self.path().display(), approval_id = %approval_id),
        err
    )]
    pub async fn remove_approval(&self, approval_id: &ApprovalId) -> GitResult<usize> {
        let _mutation = self.core.mutation.lock().await;
        let prefix = format!("{APPROVAL_REFS}/{approval_id}/");
        let refs = self.reference_names(&[&prefix]).await?;
        self.delete_references(&refs).await?;
        Ok(refs.len())
    }

    /// Copies current upstream branches and tags into an isolated receive
    /// namespace and returns the exact advertised baseline.
    ///
    /// Only refs are copied; all namespaces share this store's object
    /// database.
    ///
    /// # Errors
    ///
    /// Returns a report when the namespace already contains refs or Git cannot
    /// create its baseline.
    #[tracing::instrument(
        name = "tascarrel_git.store.stage_receive_namespace",
        level = "info",
        skip(self),
        fields(repository = %self.path().display(), namespace = %namespace),
        err
    )]
    pub async fn stage_receive_namespace(
        &self,
        namespace: &ReceiveNamespace,
    ) -> GitResult<Vec<RepositoryReference>> {
        let _mutation = self.core.mutation.lock().await;
        let prefix = receive_namespace_prefix(namespace);
        if !self.reference_names(&[&prefix]).await?.is_empty() {
            return Err(Report::new(GitError::InvalidPublication {
                reason: "receive namespace already exists",
            }));
        }
        let baseline = self.references_locked().await?;
        for reference in &baseline {
            let destination = format!("{prefix}{}", reference.name.as_str());
            let mut command = self.command();
            command
                .args(["update-ref", "--create-reflog", "--"])
                .arg(destination)
                .arg(reference.object.as_str());
            self.run(command, "stage receive-pack refs", &[])
                .await?
                .success("stage receive-pack refs")?;
        }
        Ok(baseline)
    }

    /// Returns branch and tag changes retained by a completed receive-pack.
    ///
    /// # Errors
    ///
    /// Returns a report for missing baseline refs, unsupported destinations,
    /// malformed objects, or failed ancestry inspection.
    #[tracing::instrument(
        name = "tascarrel_git.store.received_updates",
        level = "info",
        skip(self, baseline),
        fields(repository = %self.path().display(), namespace = %namespace),
        err
    )]
    pub async fn received_updates(
        &self,
        namespace: &ReceiveNamespace,
        baseline: &[RepositoryReference],
    ) -> GitResult<Vec<ReceivedReferenceUpdate>> {
        let _mutation = self.core.mutation.lock().await;
        let prefix = receive_namespace_prefix(namespace);
        let retained = self.reference_names(&[&prefix]).await?;
        let mut proposed = BTreeMap::new();
        for source in retained {
            let destination = source.as_str().strip_prefix(&prefix).ok_or_else(|| {
                Report::new(GitError::MalformedOutput {
                    action: "inspect receive-pack refs",
                })
            })?;
            let destination = ReferenceName::new(destination)?;
            if !destination.is_branch() && !destination.is_tag() {
                return Err(Report::new(GitError::UnsupportedDestination {
                    reference: destination.to_string(),
                }));
            }
            let inspected = self.inspect_reference(&source).await?;
            proposed.insert(destination, (source, inspected.object));
        }
        let previous = baseline
            .iter()
            .map(|reference| (reference.name.clone(), reference.object.clone()))
            .collect::<BTreeMap<_, _>>();
        if previous
            .keys()
            .any(|reference| !proposed.contains_key(reference))
        {
            return Err(Report::new(GitError::InvalidPublication {
                reason: "receive-pack deleted a retained ref",
            }));
        }
        let mut updates = Vec::new();
        for (destination, (source, object)) in proposed {
            let expected = previous.get(&destination).cloned();
            if expected.as_ref() == Some(&object) {
                continue;
            }
            let rewrites = if destination.is_tag() {
                expected.is_some()
            } else if let Some(expected) = &expected {
                !self.is_ancestor(expected, &object).await?
            } else {
                false
            };
            updates.push(ReceivedReferenceUpdate {
                source,
                destination,
                previous: expected,
                proposed: object,
                rewrites,
            });
        }
        Ok(updates)
    }

    /// Removes every ref retained by one receive-pack namespace.
    ///
    /// # Errors
    ///
    /// Returns a report when namespace refs cannot be enumerated or deleted.
    #[tracing::instrument(
        name = "tascarrel_git.store.remove_receive_namespace",
        level = "info",
        skip(self),
        fields(repository = %self.path().display(), namespace = %namespace),
        err
    )]
    pub async fn remove_receive_namespace(&self, namespace: &ReceiveNamespace) -> GitResult<usize> {
        let _mutation = self.core.mutation.lock().await;
        let prefix = receive_namespace_prefix(namespace);
        let refs = self.reference_names(&[&prefix]).await?;
        self.delete_references(&refs).await?;
        Ok(refs.len())
    }

    /// Resolves and inspects one retained reference.
    ///
    /// # Errors
    ///
    /// Returns a report when the ref is missing or its object is malformed.
    pub async fn resolve_reference(
        &self,
        reference: &ReferenceName,
    ) -> GitResult<RepositoryReference> {
        let inspected = self.inspect_reference(reference).await?;
        Ok(RepositoryReference {
            name: reference.clone(),
            object: inspected.object,
            kind: inspected.kind,
            peeled_commit: inspected.peeled_commit,
        })
    }

    /// Reports upstream refs and object storage retained by this repository.
    ///
    /// # Errors
    ///
    /// Returns a report when Git cannot enumerate refs or decode its bounded
    /// object statistics.
    #[tracing::instrument(
        name = "tascarrel_git.store.statistics",
        level = "debug",
        skip(self),
        fields(repository = %self.path().display()),
        err
    )]
    pub async fn statistics(&self) -> GitResult<RepositoryStatistics> {
        let references = self.references_locked().await?;
        let captures = self.reference_names(&[WORKSPACE_REFS]).await?.len();
        let mut command = self.command();
        command.args(["count-objects", "-v"]);
        let output = self
            .run(command, "inspect repository storage", &[])
            .await?
            .success("inspect repository storage")?;
        let text = std::str::from_utf8(&output).map_err(|_| {
            Report::new(GitError::MalformedOutput {
                action: "inspect repository storage",
            })
        })?;
        let values = parse_count_objects(text)?;
        let loose_objects = count_object_value(&values, "count")?;
        let packed_objects = count_object_value(&values, "in-pack")?;
        let packs = count_object_value(&values, "packs")?;
        let loose_kib = count_object_value(&values, "size")?;
        let packed_kib = count_object_value(&values, "size-pack")?;
        let garbage_kib = count_object_value(&values, "size-garbage")?;
        let size_kib = loose_kib
            .checked_add(packed_kib)
            .and_then(|size| size.checked_add(garbage_kib))
            .ok_or_else(malformed_count_objects)?;
        Ok(RepositoryStatistics {
            branches: references
                .iter()
                .filter(|reference| reference.name.is_branch())
                .count(),
            tags: references
                .iter()
                .filter(|reference| reference.name.is_tag())
                .count(),
            captures,
            loose_objects,
            packed_objects,
            packs,
            size_bytes: size_kib
                .checked_mul(1024)
                .ok_or_else(malformed_count_objects)?,
            garbage_bytes: garbage_kib
                .checked_mul(1024)
                .ok_or_else(malformed_count_objects)?,
        })
    }

    /// Compares a captured ref's peeled commit with one base commit.
    ///
    /// # Errors
    ///
    /// Returns a report when the capture does not peel to a commit or Git
    /// cannot calculate ancestry and diff statistics.
    #[tracing::instrument(
        name = "tascarrel_git.store.compare",
        level = "debug",
        skip(self),
        fields(repository = %self.path().display(), capture = %capture.retained_as),
        err
    )]
    pub async fn compare(
        &self,
        base: &ObjectId,
        capture: &CapturedReference,
    ) -> GitResult<ReferenceComparison> {
        let head = capture.peeled_commit.as_ref().ok_or_else(|| {
            Report::new(GitError::NotCommit {
                reference: capture.retained_as.to_string(),
            })
        })?;
        let fast_forward = self.is_ancestor(base, head).await?;
        let commits = self.commit_count(base, head).await?;
        let (files, insertions, deletions, binary_files) = self.diff_stats(base, head).await?;
        Ok(ReferenceComparison {
            fast_forward,
            commits,
            files,
            insertions,
            deletions,
            binary_files,
        })
    }

    /// Lists commits reachable from one proposed object but not its previous
    /// object.
    ///
    /// Both objects may be commits or tags which peel to commits. A missing
    /// previous object represents a newly created reference and therefore
    /// returns the complete proposed history.
    ///
    /// # Errors
    ///
    /// Returns a report when an object does not peel to a commit, Git fails,
    /// or the bounded command output is exceeded.
    #[tracing::instrument(
        name = "tascarrel_git.store.commits_between",
        level = "debug",
        skip(self),
        fields(repository = %self.path().display()),
        err
    )]
    pub async fn commits_between(
        &self,
        previous: Option<&ObjectId>,
        proposed: &ObjectId,
    ) -> GitResult<Vec<GitCommit>> {
        let proposed = self.require_peeled_commit(proposed).await?;
        let previous = match previous {
            Some(previous) => Some(self.require_peeled_commit(previous).await?),
            None => None,
        };
        let mut command = self.command();
        command.args([
            "log",
            "--reverse",
            "--topo-order",
            "--no-show-signature",
            "--format=%H%x00%P%x00%an%x00%ae%x00%aI%x00%cn%x00%ce%x00%cI%x00%s%x00%b%x00",
        ]);
        command.arg(proposed.as_str());
        if let Some(previous) = &previous {
            command.arg(format!("^{}", previous.as_str()));
        }
        let output = self
            .run(command, "list repository approval commits", &[])
            .await?
            .success("list repository approval commits")?;
        parse_commits(&output)
    }

    /// Returns whether one commit belongs to an exact previous/proposed
    /// comparison.
    ///
    /// # Errors
    ///
    /// Returns a report when a comparison endpoint does not peel to a commit
    /// or Git cannot inspect ancestry. An unavailable candidate returns
    /// `false`.
    #[tracing::instrument(
        name = "tascarrel_git.store.commit_is_between",
        level = "debug",
        skip(self),
        fields(repository = %self.path().display()),
        err
    )]
    pub async fn commit_is_between(
        &self,
        previous: Option<&ObjectId>,
        proposed: &ObjectId,
        commit: &ObjectId,
    ) -> GitResult<bool> {
        let proposed = self.require_peeled_commit(proposed).await?;
        let Some(commit) = self
            .rev_parse(&format!("{}^{{commit}}", commit.as_str()))
            .await?
        else {
            return Ok(false);
        };
        if !self.is_ancestor(&commit, &proposed).await? {
            return Ok(false);
        }
        let Some(previous) = previous else {
            return Ok(true);
        };
        let previous = self.require_peeled_commit(previous).await?;
        Ok(!self.is_ancestor(&commit, &previous).await?)
    }

    /// Builds the unified diff introduced by one exact commit.
    ///
    /// Merge commits are compared with their first parent. Root commits are
    /// compared with the empty tree.
    ///
    /// # Errors
    ///
    /// Returns a report when the object is not a commit, Git fails, or the
    /// bounded command output is exceeded.
    #[tracing::instrument(
        name = "tascarrel_git.store.commit_diff",
        level = "debug",
        skip(self),
        fields(repository = %self.path().display()),
        err
    )]
    pub async fn commit_diff(&self, commit: &ObjectId) -> GitResult<String> {
        let commit = self.require_peeled_commit(commit).await?;
        let parent = self.rev_parse(&format!("{}^1", commit.as_str())).await?;
        let mut command = self.command();
        if let Some(parent) = parent {
            command.args([
                "diff",
                "--no-ext-diff",
                "--no-textconv",
                "--no-color",
                "--find-renames",
                "--find-copies",
            ]);
            command.arg(parent.as_str()).arg(commit.as_str()).arg("--");
        } else {
            command.args([
                "diff-tree",
                "--root",
                "--no-commit-id",
                "--no-ext-diff",
                "--no-textconv",
                "--no-color",
                "--find-renames",
                "--find-copies",
                "-p",
            ]);
            command.arg(commit.as_str()).arg("--");
        }
        let output = self
            .run(command, "generate repository approval commit diff", &[])
            .await?
            .success("generate repository approval commit diff")?;
        String::from_utf8(output).map_err(|_| {
            Report::new(GitError::MalformedOutput {
                action: "generate repository approval commit diff",
            })
        })
    }

    /// Atomically publishes one approved set of branch and tag updates.
    ///
    /// Every destination is protected by its explicit expected value. An
    /// upstream ref already at the approved source is treated as an idempotent
    /// success. Branch rewrites and existing-tag changes require explicit
    /// authorization in the corresponding [`RefUpdate`].
    ///
    /// # Errors
    ///
    /// Returns a typed conflict for a changed lease, a policy error for an
    /// unauthorized rewrite, or a Git report for transport and remote
    /// rejection failures.
    #[tracing::instrument(
        name = "tascarrel_git.store.publish",
        level = "info",
        skip(self, remote, updates),
        fields(repository = %self.path().display(), references = updates.len()),
        err
    )]
    pub async fn publish(
        &self,
        remote: &Remote,
        updates: &[RefUpdate],
    ) -> GitResult<PublishOutcome> {
        validate_publication(updates)?;
        let _mutation = self.core.mutation.lock().await;
        self.refresh_locked(remote).await?;

        let destinations = updates
            .iter()
            .map(RefUpdate::destination)
            .collect::<Vec<_>>();
        let upstream = self.remote_references(remote, &destinations).await?;
        let mut prepared = Vec::with_capacity(updates.len());
        let mut pending = Vec::new();
        for update in updates {
            let source = self.inspect_reference(update.source()).await?;
            validate_source(update, &source)?;
            let actual = upstream.get(update.destination()).cloned();
            let already_present = actual.as_ref() == Some(&source.object);
            if !already_present && actual.as_ref() != update.expected() {
                return Err(lease_conflict(update, actual.as_ref()));
            }
            if !already_present {
                self.validate_rewrite(update, actual.as_ref(), &source)
                    .await?;
                pending.push((update, source.object.clone()));
            }
            prepared.push(PublishedReference {
                reference: update.destination().clone(),
                object: source.object,
                already_present,
            });
        }

        if pending.is_empty() {
            return Ok(PublishOutcome {
                references: prepared,
                changed: false,
            });
        }

        let mut command = self.command();
        command.args(["push", "--porcelain", "--atomic", "--no-verify"]);
        for (update, _) in &pending {
            command.arg(format!(
                "--force-with-lease={}:{}",
                update.destination(),
                update.expected().map_or("", ObjectId::as_str)
            ));
        }
        command.arg("--").arg(remote.as_str());
        for (update, _) in &pending {
            command.arg(format!("{}:{}", update.source(), update.destination()));
        }
        let output = self
            .run(command, "publish approved refs", &[remote.as_str()])
            .await?;
        if !output.status.success() {
            let observed = self.remote_references(remote, &destinations).await?;
            if updates.iter().all(|update| {
                let expected_source = prepared
                    .iter()
                    .find(|published| published.reference == *update.destination())
                    .map(|published| &published.object);
                observed.get(update.destination()) == expected_source
            }) {
                return Ok(PublishOutcome {
                    references: prepared,
                    changed: true,
                });
            }
            for update in updates {
                let actual = observed.get(update.destination());
                if actual != update.expected() {
                    return Err(lease_conflict(update, actual));
                }
            }
            return Err(output.failure("publish approved refs"));
        }

        Ok(PublishOutcome {
            references: prepared,
            changed: true,
        })
    }

    /// Runs bounded incremental maintenance on the managed object store.
    ///
    /// # Errors
    ///
    /// Returns a report when Git cannot update commit graphs, pack loose
    /// objects, inspect the resulting pack directory, or perform an
    /// incremental repack. Repacking is skipped when the store has no packs.
    #[tracing::instrument(
        name = "tascarrel_git.store.maintain",
        level = "debug",
        skip(self),
        fields(repository = %self.path().display()),
        err
    )]
    pub async fn maintain(&self) -> GitResult<()> {
        let _mutation = self.core.mutation.lock().await;
        let mut command = self.command();
        command.args([
            "maintenance",
            "run",
            "--quiet",
            "--task=commit-graph",
            "--task=loose-objects",
        ]);
        self.run(command, "maintain the managed repository", &[])
            .await?
            .success("maintain the managed repository")?;
        if !has_pack_files(&self.core.path)? {
            return Ok(());
        }
        let mut command = self.command();
        command.args(["maintenance", "run", "--quiet", "--task=incremental-repack"]);
        self.run(command, "incrementally repack the managed repository", &[])
            .await?
            .success("incrementally repack the managed repository")?;
        Ok(())
    }

    pub(crate) fn command(&self) -> Command {
        let mut command = self.core.git.command();
        command.arg("-C").arg(&self.core.path);
        command
    }

    pub(crate) async fn run(
        &self,
        command: Command,
        action: &'static str,
        redactions: &[&str],
    ) -> GitResult<GitCommandOutput> {
        self.core
            .git
            .run(command, action, &self.core.limits, redactions)
            .await
    }

    pub(crate) fn limits(&self) -> &GitLimits {
        &self.core.limits
    }

    async fn remote_default_branch(&self, remote: &Remote) -> GitResult<Option<ReferenceName>> {
        let mut command = self.core.git.command();
        command.args(["ls-remote", "--symref", "--"]);
        command.arg(remote.as_str()).arg("HEAD");
        let output = self
            .run(
                command,
                "resolve the upstream default branch",
                &[remote.as_str()],
            )
            .await?
            .success("resolve the upstream default branch")?;
        let text = std::str::from_utf8(&output).map_err(|_| {
            Report::new(GitError::MalformedOutput {
                action: "resolve the upstream default branch",
            })
        })?;
        for line in text.lines() {
            if let Some(reference) = line
                .strip_prefix("ref: ")
                .and_then(|line| line.strip_suffix("\tHEAD"))
            {
                let reference = ReferenceName::new(reference)?;
                if reference.is_branch() {
                    return Ok(Some(reference));
                }
            }
        }
        Ok(None)
    }

    async fn configure(&self, key: &str, value: &str) -> GitResult<()> {
        let mut command = self.command();
        command.args(["config", "--local", "--", key, value]);
        self.run(command, "configure the managed repository", &[])
            .await?
            .success("configure the managed repository")?;
        Ok(())
    }

    async fn refresh_locked(&self, remote: &Remote) -> GitResult<Option<ReferenceName>> {
        let default_branch = self.remote_default_branch(remote).await?;
        let mut command = self.command();
        command.args([
            "fetch",
            "--quiet",
            "--no-auto-gc",
            "--prune",
            "--force",
            "--no-write-fetch-head",
            "--no-tags",
            "--",
        ]);
        command
            .arg(remote.as_str())
            .args(["+refs/heads/*:refs/heads/*", "+refs/tags/*:refs/tags/*"]);
        self.run(command, "refresh upstream refs", &[remote.as_str()])
            .await?
            .success("refresh upstream refs")?;
        let mut command = self.command();
        command.args(["symbolic-ref", "HEAD"]).arg(
            default_branch
                .as_ref()
                .map_or(NO_DEFAULT_BRANCH_REF, ReferenceName::as_str),
        );
        self.run(command, "update the cached default branch", &[])
            .await?
            .success("update the cached default branch")?;
        Ok(default_branch)
    }

    async fn references_locked(&self) -> GitResult<Vec<RepositoryReference>> {
        let mut command = self.command();
        command.args([
            "for-each-ref",
            "--format=%(refname)%00%(objectname)%00%(objecttype)%00%(*objectname)%00%(*objecttype)",
            "refs/heads/",
            "refs/tags/",
        ]);
        let output = self
            .run(command, "list repository refs", &[])
            .await?
            .success("list repository refs")?;
        let text = std::str::from_utf8(&output).map_err(|_| {
            Report::new(GitError::MalformedOutput {
                action: "list repository refs",
            })
        })?;
        let mut references = Vec::new();
        for line in text.lines().filter(|line| !line.is_empty()) {
            if references.len() == self.core.limits.references {
                return Err(Report::new(GitError::ReferenceLimit {
                    action: "list repository refs",
                    limit: self.core.limits.references,
                }));
            }
            let mut fields = line.split('\0');
            let name = fields.next();
            let object = fields.next();
            let kind = fields.next();
            let peeled_object = fields.next();
            let peeled_kind = fields.next();
            if fields.next().is_some() {
                return Err(Report::new(GitError::MalformedOutput {
                    action: "list repository refs",
                }));
            }
            let (Some(name), Some(object), Some(kind), Some(peeled_object), Some(peeled_kind)) =
                (name, object, kind, peeled_object, peeled_kind)
            else {
                return Err(Report::new(GitError::MalformedOutput {
                    action: "list repository refs",
                }));
            };
            let name = ReferenceName::new(name)?;
            let object = ObjectId::new(object)?;
            let kind = parse_object_kind(kind)?;
            let peeled_commit = if kind == ObjectKind::Commit {
                Some(object.clone())
            } else if peeled_kind == "commit" {
                Some(ObjectId::new(peeled_object)?)
            } else {
                None
            };
            references.push(RepositoryReference {
                name,
                object,
                kind,
                peeled_commit,
            });
        }
        Ok(references)
    }

    async fn inspect_reference(&self, reference: &ReferenceName) -> GitResult<InspectedRef> {
        let object = self.rev_parse(reference.as_str()).await?.ok_or_else(|| {
            Report::new(GitError::MissingReference {
                reference: reference.to_string(),
            })
        })?;
        let kind = self.object_kind(&object).await?;
        let peeled_commit = self.peel_commit(reference).await?;
        Ok(InspectedRef {
            object,
            kind,
            peeled_commit,
        })
    }

    async fn rev_parse(&self, expression: &str) -> GitResult<Option<ObjectId>> {
        let mut command = self.command();
        command.args(["rev-parse", "--verify", "--quiet", expression]);
        let output = self.run(command, "resolve a Git object", &[]).await?;
        match output.status.code() {
            Some(0) => {}
            Some(1) => {
                tracing::debug!(expression, "Git object did not resolve");
                return Ok(None);
            }
            _ => return Err(output.failure("resolve a Git object")),
        }
        let value = output.stdout_text("resolve a Git object")?.trim();
        ObjectId::new(value).map(Some)
    }

    async fn object_kind(&self, object: &ObjectId) -> GitResult<ObjectKind> {
        let mut command = self.command();
        command.args(["cat-file", "-t"]).arg(object.as_str());
        let output = self
            .run(command, "inspect a Git object", &[])
            .await?
            .success("inspect a Git object")?;
        let value = std::str::from_utf8(&output).map_err(|_| {
            Report::new(GitError::MalformedOutput {
                action: "inspect a Git object",
            })
        })?;
        parse_object_kind(value.trim())
    }

    async fn peel_commit(&self, reference: &ReferenceName) -> GitResult<Option<ObjectId>> {
        self.rev_parse(&format!("{}^{{commit}}", reference.as_str()))
            .await
    }

    /// Resolves an object to its commit target or returns `NotCommit`.
    async fn require_peeled_commit(&self, object: &ObjectId) -> GitResult<ObjectId> {
        self.rev_parse(&format!("{}^{{commit}}", object.as_str()))
            .await?
            .ok_or_else(|| {
                Report::new(GitError::NotCommit {
                    reference: object.to_string(),
                })
            })
    }

    async fn is_ancestor(&self, ancestor: &ObjectId, descendant: &ObjectId) -> GitResult<bool> {
        let mut command = self.command();
        command
            .args(["merge-base", "--is-ancestor"])
            .arg(ancestor.as_str())
            .arg(descendant.as_str());
        let output = self.run(command, "check Git ancestry", &[]).await?;
        match output.status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(output.failure("check Git ancestry")),
        }
    }

    async fn commit_count(&self, base: &ObjectId, head: &ObjectId) -> GitResult<u64> {
        let mut command = self.command();
        command
            .args(["rev-list", "--count"])
            .arg(format!("{}..{}", base.as_str(), head.as_str()))
            .arg("--");
        let output = self
            .run(command, "count captured commits", &[])
            .await?
            .success("count captured commits")?;
        std::str::from_utf8(&output)
            .ok()
            .and_then(|value| value.trim().parse().ok())
            .ok_or_else(|| {
                Report::new(GitError::MalformedOutput {
                    action: "count captured commits",
                })
            })
    }

    async fn diff_stats(
        &self,
        base: &ObjectId,
        head: &ObjectId,
    ) -> GitResult<(u64, u64, u64, u64)> {
        let mut command = self.command();
        command
            .args(["diff", "--numstat", "--no-renames"])
            .arg(format!("{}...{}", base.as_str(), head.as_str()))
            .arg("--");
        let output = self
            .run(command, "calculate captured diff statistics", &[])
            .await?
            .success("calculate captured diff statistics")?;
        let text = std::str::from_utf8(&output).map_err(|_| {
            Report::new(GitError::MalformedOutput {
                action: "calculate captured diff statistics",
            })
        })?;
        let mut files = 0_u64;
        let mut insertions = 0_u64;
        let mut deletions = 0_u64;
        let mut binary_files = 0_u64;
        for line in text.lines().filter(|line| !line.is_empty()) {
            let mut fields = line.splitn(3, '\t');
            let additions = fields.next();
            let removals = fields.next();
            let path = fields.next();
            let (Some(additions), Some(removals), Some(_)) = (additions, removals, path) else {
                return Err(Report::new(GitError::MalformedOutput {
                    action: "calculate captured diff statistics",
                }));
            };
            files += 1;
            if additions == "-" && removals == "-" {
                binary_files += 1;
                continue;
            }
            insertions = insertions
                .checked_add(parse_stat(additions)?)
                .ok_or_else(|| {
                    Report::new(GitError::MalformedOutput {
                        action: "calculate captured diff statistics",
                    })
                })?;
            deletions = deletions
                .checked_add(parse_stat(removals)?)
                .ok_or_else(|| {
                    Report::new(GitError::MalformedOutput {
                        action: "calculate captured diff statistics",
                    })
                })?;
        }
        Ok((files, insertions, deletions, binary_files))
    }

    async fn remote_references(
        &self,
        remote: &Remote,
        references: &[&ReferenceName],
    ) -> GitResult<BTreeMap<ReferenceName, ObjectId>> {
        let mut command = self.core.git.command();
        command.args(["ls-remote", "--refs", "--"]);
        command.arg(remote.as_str());
        for reference in references {
            command.arg(reference.as_str());
        }
        let output = self
            .run(command, "query upstream refs", &[remote.as_str()])
            .await?
            .success("query upstream refs")?;
        let text = std::str::from_utf8(&output).map_err(|_| {
            Report::new(GitError::MalformedOutput {
                action: "query upstream refs",
            })
        })?;
        let mut parsed = BTreeMap::new();
        for line in text.lines().filter(|line| !line.is_empty()) {
            if parsed.len() == self.core.limits.references {
                return Err(Report::new(GitError::ReferenceLimit {
                    action: "query upstream refs",
                    limit: self.core.limits.references,
                }));
            }
            let Some((object, reference)) = line.split_once('\t') else {
                return Err(Report::new(GitError::MalformedOutput {
                    action: "query upstream refs",
                }));
            };
            parsed.insert(ReferenceName::new(reference)?, ObjectId::new(object)?);
        }
        Ok(parsed)
    }

    async fn validate_rewrite(
        &self,
        update: &RefUpdate,
        actual: Option<&ObjectId>,
        source: &InspectedRef,
    ) -> GitResult<()> {
        let Some(actual) = actual else {
            return Ok(());
        };
        if update.destination().is_tag() {
            if update.allows_rewrite() {
                return Ok(());
            }
            return Err(Report::new(GitError::TagExists {
                reference: update.destination().to_string(),
            }));
        }
        let head = source.peeled_commit.as_ref().ok_or_else(|| {
            Report::new(GitError::NotCommit {
                reference: update.source().to_string(),
            })
        })?;
        if update.allows_rewrite() || self.is_ancestor(actual, head).await? {
            return Ok(());
        }
        Err(Report::new(GitError::NonFastForward {
            reference: update.destination().to_string(),
        }))
    }

    async fn reference_names(&self, prefixes: &[&str]) -> GitResult<Vec<ReferenceName>> {
        let mut command = self.command();
        command.args(["for-each-ref", "--format=%(refname)"]);
        command.args(prefixes);
        let output = self
            .run(command, "list internal Git refs", &[])
            .await?
            .success("list internal Git refs")?;
        let text = std::str::from_utf8(&output).map_err(|_| {
            Report::new(GitError::MalformedOutput {
                action: "list internal Git refs",
            })
        })?;
        let refs = text
            .lines()
            .filter(|line| !line.is_empty())
            .map(ReferenceName::new)
            .collect::<GitResult<Vec<_>>>()?;
        if refs.len() > self.core.limits.references {
            return Err(Report::new(GitError::ReferenceLimit {
                action: "list internal Git refs",
                limit: self.core.limits.references,
            }));
        }
        Ok(refs)
    }

    async fn delete_references(&self, references: &[ReferenceName]) -> GitResult<()> {
        for reference in references {
            let mut command = self.command();
            command
                .args(["update-ref", "-d", "--"])
                .arg(reference.as_str());
            self.run(command, "delete captured refs", &[])
                .await?
                .success("delete captured refs")?;
        }
        Ok(())
    }
}

#[derive(Debug)]
struct RepositoryStoreCore {
    git: GitBinary,
    path: PathBuf,
    limits: GitLimits,
    mutation: Mutex<()>,
}

struct InspectedRef {
    object: ObjectId,
    kind: ObjectKind,
    peeled_commit: Option<ObjectId>,
}

fn validate_limits(limits: &GitLimits) -> GitResult<()> {
    if limits.diagnostic_bytes == 0 || limits.command_output_bytes == 0 || limits.references == 0 {
        return Err(Report::new(GitError::InvalidLimits));
    }
    Ok(())
}

fn receive_namespace_prefix(namespace: &ReceiveNamespace) -> String {
    format!("refs/namespaces/{namespace}/")
}

fn has_pack_files(repository: &Path) -> GitResult<bool> {
    let entries = fs::read_dir(repository.join("objects/pack")).map_err(|source| {
        Report::new(GitError::Io {
            action: "inspect the managed Git pack directory",
            source,
        })
    })?;
    for entry in entries {
        let entry = entry.map_err(|source| {
            Report::new(GitError::Io {
                action: "inspect the managed Git pack directory",
                source,
            })
        })?;
        if entry
            .path()
            .extension()
            .is_some_and(|extension| extension == "pack")
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Installs the immutable branch-and-tag allowlist used by receive-pack.
fn install_receive_update_hook(repository: &Path, hooks: &Path) -> GitResult<()> {
    let hook = hooks.join("update");
    match fs::symlink_metadata(&hook) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink()
                || !metadata.is_file()
                || usize::try_from(metadata.len()).ok() != Some(RECEIVE_UPDATE_HOOK.len())
            {
                return Err(Report::new(GitError::InvalidRepository {
                    path: repository.to_owned(),
                }));
            }
            let contents = fs::read(&hook).map_err(|source| {
                Report::new(GitError::Io {
                    action: "read the managed receive hook",
                    source,
                })
            })?;
            if contents != RECEIVE_UPDATE_HOOK {
                return Err(Report::new(GitError::InvalidRepository {
                    path: repository.to_owned(),
                }));
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(0o700)
                .open(&hook)
                .map_err(|source| {
                    Report::new(GitError::Io {
                        action: "create the managed receive hook",
                        source,
                    })
                })?;
            file.write_all(RECEIVE_UPDATE_HOOK).map_err(|source| {
                Report::new(GitError::Io {
                    action: "write the managed receive hook",
                    source,
                })
            })?;
            file.sync_all().map_err(|source| {
                Report::new(GitError::Io {
                    action: "sync the managed receive hook",
                    source,
                })
            })?;
        }
        Err(source) => {
            return Err(Report::new(GitError::Io {
                action: "inspect the managed receive hook",
                source,
            }));
        }
    }
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o700)).map_err(|source| {
        Report::new(GitError::Io {
            action: "set managed receive hook permissions",
            source,
        })
    })?;
    Ok(())
}

fn validate_bare_repository(path: &Path) -> GitResult<()> {
    if !path.join("HEAD").is_file() || !path.join("objects").is_dir() || !path.join("refs").is_dir()
    {
        return Err(Report::new(GitError::InvalidRepository {
            path: path.to_owned(),
        }));
    }
    Ok(())
}

fn validate_repository_directory(path: &Path) -> GitResult<()> {
    if !path.is_absolute() {
        return Err(Report::new(GitError::InvalidRepositoryPath {
            path: path.to_owned(),
        }));
    }
    let metadata = fs::symlink_metadata(path).map_err(|source| {
        Report::new(GitError::Io {
            action: "inspect the managed Git repository",
            source,
        })
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(Report::new(GitError::InvalidRepository {
            path: path.to_owned(),
        }));
    }
    validate_bare_repository(path)
}

fn capture_reference(
    workspace_id: &WorkspaceId,
    pod_id: &PodId,
    capture_id: &CaptureId,
    source: &SourceReference,
) -> GitResult<ReferenceName> {
    ReferenceName::new(format!(
        "{WORKSPACE_REFS}/{}/pods/{}/captures/{}/{}",
        workspace_id.as_str(),
        pod_id.as_str(),
        capture_id.as_str(),
        source.as_str()
    ))
}

/// Builds the hidden ref retaining one approval update's baseline.
fn approval_base_reference(
    approval_id: &ApprovalId,
    update_index: usize,
) -> GitResult<ReferenceName> {
    ReferenceName::new(format!(
        "{APPROVAL_REFS}/{}/bases/{update_index}",
        approval_id.as_str()
    ))
}

/// Parses the NUL-delimited records emitted by `commits_between`.
fn parse_commits(output: &[u8]) -> GitResult<Vec<GitCommit>> {
    let mut commits = Vec::new();
    let mut fields = output.split(|byte| *byte == 0);
    while let Some(id) = fields.next() {
        let id = id.strip_prefix(b"\n").unwrap_or(id);
        if id.iter().all(u8::is_ascii_whitespace) {
            if fields.all(|field| field.iter().all(u8::is_ascii_whitespace)) {
                break;
            }
            return Err(malformed_commit_metadata());
        }
        let mut record = Vec::with_capacity(10);
        record.push(id);
        for _ in 1..10 {
            record.push(fields.next().ok_or_else(malformed_commit_metadata)?);
        }
        let parents = utf8_field(record[1])?
            .split_whitespace()
            .map(ObjectId::new)
            .collect::<GitResult<Vec<_>>>()?;
        commits.push(GitCommit {
            id: ObjectId::new(utf8_field(record[0])?.trim())?,
            parents,
            author: GitSignature {
                name: utf8_field(record[2])?.to_owned(),
                email: utf8_field(record[3])?.to_owned(),
                timestamp: utf8_field(record[4])?.to_owned(),
            },
            committer: GitSignature {
                name: utf8_field(record[5])?.to_owned(),
                email: utf8_field(record[6])?.to_owned(),
                timestamp: utf8_field(record[7])?.to_owned(),
            },
            subject: utf8_field(record[8])?.to_owned(),
            body: utf8_field(record[9])?.to_owned(),
        });
    }
    Ok(commits)
}

/// Validates one textual field in Git's structured commit output.
fn utf8_field(bytes: &[u8]) -> GitResult<&str> {
    std::str::from_utf8(bytes).map_err(|_| malformed_commit_metadata())
}

/// Reports malformed structured commit output consistently.
fn malformed_commit_metadata() -> Report<GitError> {
    Report::new(GitError::MalformedOutput {
        action: "list repository approval commits",
    })
}

fn parse_count_objects(output: &str) -> GitResult<BTreeMap<&str, u64>> {
    output
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            let (name, value) = line.split_once(": ").ok_or_else(malformed_count_objects)?;
            let value = value.parse().map_err(|_| malformed_count_objects())?;
            Ok((name, value))
        })
        .collect()
}

fn count_object_value(values: &BTreeMap<&str, u64>, name: &str) -> GitResult<u64> {
    values
        .get(name)
        .copied()
        .ok_or_else(malformed_count_objects)
}

fn malformed_count_objects() -> Report<GitError> {
    Report::new(GitError::MalformedOutput {
        action: "inspect repository storage",
    })
}

fn parse_object_kind(value: &str) -> GitResult<ObjectKind> {
    match value {
        "commit" => Ok(ObjectKind::Commit),
        "tag" => Ok(ObjectKind::Tag),
        "tree" => Ok(ObjectKind::Tree),
        "blob" => Ok(ObjectKind::Blob),
        _ => Err(Report::new(GitError::MalformedOutput {
            action: "inspect a Git object",
        })),
    }
}

fn parse_stat(value: &str) -> GitResult<u64> {
    value.parse().map_err(|_| {
        Report::new(GitError::MalformedOutput {
            action: "calculate captured diff statistics",
        })
    })
}

fn validate_publication(updates: &[RefUpdate]) -> GitResult<()> {
    if updates.is_empty() {
        return Err(Report::new(GitError::InvalidPublication {
            reason: "at least one ref update is required",
        }));
    }
    let mut destinations = BTreeSet::new();
    if updates
        .iter()
        .any(|update| !destinations.insert(update.destination()))
    {
        return Err(Report::new(GitError::InvalidPublication {
            reason: "destination refs must be unique",
        }));
    }
    Ok(())
}

fn validate_source(update: &RefUpdate, source: &InspectedRef) -> GitResult<()> {
    if update.destination().is_branch() && source.kind != ObjectKind::Commit {
        return Err(Report::new(GitError::NotCommit {
            reference: update.source().to_string(),
        }));
    }
    if update.destination().is_tag() && source.peeled_commit.is_none() {
        return Err(Report::new(GitError::NotCommit {
            reference: update.source().to_string(),
        }));
    }
    Ok(())
}

fn lease_conflict(update: &RefUpdate, actual: Option<&ObjectId>) -> Report<GitError> {
    Report::new(GitError::LeaseConflict {
        reference: update.destination().to_string(),
        expected: update.expected().map(ToString::to_string),
        actual: actual.map(ToString::to_string),
    })
}
