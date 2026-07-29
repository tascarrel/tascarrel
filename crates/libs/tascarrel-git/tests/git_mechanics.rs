//! End-to-end managed Git store, transport, capture, and publication behavior.

use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use tascarrel_git::CaptureId;
use tascarrel_git::GitBinary;
use tascarrel_git::GitError;
use tascarrel_git::ObjectId;
use tascarrel_git::ObjectKind;
use tascarrel_git::PodId;
use tascarrel_git::ReceiveNamespace;
use tascarrel_git::RefUpdate;
use tascarrel_git::ReferenceName;
use tascarrel_git::Remote;
use tascarrel_git::RepositoryStore;
use tascarrel_git::SourceReference;
use tascarrel_git::WorkspaceId;
use tempfile::TempDir;
use tokio::io::AsyncReadExt as _;
use tokio::io::AsyncWriteExt as _;

struct Fixture {
    temporary: TempDir,
    git: GitBinary,
    upstream: PathBuf,
    source: PathBuf,
    store: RepositoryStore,
    upstream_remote: Remote,
    initial: ObjectId,
}

impl Fixture {
    async fn new() -> Self {
        let temporary = tempfile::tempdir().unwrap();
        let git = isolated_git();
        let upstream = temporary.path().join("upstream.git");
        let source = temporary.path().join("source");
        let cache = temporary.path().join("cache.git");

        run_git(&git, temporary.path(), &["init", "--bare", path(&upstream)]);
        run_git(&git, temporary.path(), &["init", path(&source)]);
        run_git(&git, &source, &["config", "user.name", "Tascarrel Test"]);
        run_git(
            &git,
            &source,
            &["config", "user.email", "tascarrel@example.invalid"],
        );
        fs::write(source.join("README.md"), "initial\n").unwrap();
        run_git(&git, &source, &["add", "README.md"]);
        run_git(&git, &source, &["commit", "-m", "initial"]);
        run_git(&git, &source, &["branch", "-M", "main"]);
        run_git(&git, &source, &["remote", "add", "origin", path(&upstream)]);
        run_git(&git, &source, &["push", "-u", "origin", "main"]);
        run_git(
            &git,
            &upstream,
            &["symbolic-ref", "HEAD", "refs/heads/main"],
        );

        let initial = object_id(&git, &source, "HEAD");
        let upstream_remote = Remote::new(path(&upstream)).unwrap();
        let store = RepositoryStore::open(git.clone(), cache).await.unwrap();
        store.refresh(&upstream_remote).await.unwrap();
        Self {
            temporary,
            git,
            upstream,
            source,
            store,
            upstream_remote,
            initial,
        }
    }

    fn clone_pod(&self, name: &str) -> PathBuf {
        let pod = self
            .temporary
            .path()
            .join(format!("pod-{}", name.replace('_', "-")));
        run_git(
            &self.git,
            self.temporary.path(),
            &["clone", path(&self.upstream), path(&pod)],
        );
        run_git(&self.git, &pod, &["config", "user.name", "Tascarrel Test"]);
        run_git(
            &self.git,
            &pod,
            &["config", "user.email", "tascarrel@example.invalid"],
        );
        pod
    }
}

/// Verifies read-only inspection never initializes a missing cache.
#[test]
fn opening_an_existing_store_does_not_create_it() {
    let temporary = tempfile::tempdir().unwrap();
    let cache = temporary.path().join("missing.git");
    let report = RepositoryStore::open_existing(isolated_git(), &cache)
        .expect_err("a missing cache must remain missing");

    assert!(matches!(report.error(), GitError::Io { .. }));
    assert!(!cache.exists());
}

/// Verifies maintenance remains a no-op when an empty store has no packs to
/// index.
#[tokio::test]
async fn maintenance_accepts_a_store_without_pack_files() {
    let temporary = tempfile::tempdir().unwrap();
    let store = RepositoryStore::open(isolated_git(), temporary.path().join("cache.git"))
        .await
        .unwrap();

    store.maintain().await.unwrap();
}

/// Verifies an empty upstream is retained as a successful snapshot without an
/// advertised default branch.
#[tokio::test]
async fn refresh_accepts_an_empty_upstream() {
    let temporary = tempfile::tempdir().unwrap();
    let git = isolated_git();
    let upstream = temporary.path().join("upstream.git");
    let cache = temporary.path().join("cache.git");
    run_git(&git, temporary.path(), &["init", "--bare", path(&upstream)]);
    let remote = Remote::new(path(&upstream)).unwrap();
    let store = RepositoryStore::open(git, cache).await.unwrap();

    let refresh = store.refresh_snapshot(&remote).await.unwrap();

    assert_eq!(refresh.default_branch, None);
    assert!(refresh.references.is_empty());
    assert_eq!(store.default_branch(&remote).await.unwrap(), None);
}

/// Verifies a dangling upstream `HEAD` does not prevent other branches from
/// being refreshed.
#[tokio::test]
async fn refresh_accepts_a_nonexistent_default_branch() {
    let fixture = Fixture::new().await;
    run_git(
        &fixture.git,
        &fixture.upstream,
        &["symbolic-ref", "HEAD", "refs/heads/missing"],
    );

    let refresh = fixture
        .store
        .refresh_snapshot(&fixture.upstream_remote)
        .await
        .unwrap();

    assert_eq!(refresh.default_branch, None);
    assert_eq!(refresh.references.len(), 1);
    assert_eq!(refresh.references[0].name.as_str(), "refs/heads/main");
    assert_eq!(fixture.store.cached_default_branch().await.unwrap(), None);
}

/// Verifies a refreshed cache advertises and checks out the upstream default
/// branch when cloned through upload-pack.
#[tokio::test]
async fn refresh_preserves_the_upstream_default_branch() {
    let fixture = Fixture::new().await;
    let checkout = fixture.temporary.path().join("cache-checkout");
    let remote = format!("file://{}", fixture.store.path().display());

    assert_eq!(
        fixture
            .store
            .cached_default_branch()
            .await
            .unwrap()
            .as_ref()
            .map(ReferenceName::as_str),
        Some("refs/heads/main")
    );
    run_git(
        &fixture.git,
        fixture.temporary.path(),
        &["clone", "--no-local", &remote, path(&checkout)],
    );
    assert_eq!(
        git_output(
            &fixture.git,
            &checkout,
            &["symbolic-ref", "--short", "HEAD"]
        )
        .trim(),
        "main"
    );
    assert_eq!(object_id(&fixture.git, &checkout, "HEAD"), fixture.initial);
    assert_eq!(
        fs::read_to_string(checkout.join("README.md")).unwrap(),
        "initial\n"
    );
}

/// Verifies a disconnected client cannot leave upload-pack waiting forever.
#[tokio::test]
async fn upload_pack_relay_finishes_after_client_disconnect() {
    let fixture = Fixture::new().await;
    let service = fixture.store.upload_pack().unwrap();
    let (client, server) = tokio::io::duplex(1024 * 1024);
    let relay = tokio::spawn(service.relay(server));
    drop(client);

    tokio::time::timeout(Duration::from_secs(5), relay)
        .await
        .expect("upload-pack relay did not finish after client disconnect")
        .expect("upload-pack relay task failed")
        .expect_err("disconnecting a Git client should fail the relay");
}

/// Verifies an incremental capture can be compared, published, and retried
/// without creating another object store.
#[tokio::test]
async fn captures_compares_publishes_and_retries_a_branch() {
    let fixture = Fixture::new().await;
    let pod = fixture.clone_pod("branch");
    fs::write(pod.join("README.md"), "initial\nchanged\n").unwrap();
    fs::write(pod.join("new.txt"), "new\n").unwrap();
    run_git(&fixture.git, &pod, &["add", "README.md", "new.txt"]);
    run_git(&fixture.git, &pod, &["commit", "-m", "change"]);

    let capture = fixture
        .store
        .import_capture(
            &Remote::new(path(&pod)).unwrap(),
            &WorkspaceId::new("workspace").unwrap(),
            &PodId::new("pod_branch").unwrap(),
            &SourceReference::new("refs/heads/main").unwrap(),
            &CaptureId::new("cap_branch").unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(capture.kind, ObjectKind::Commit);
    assert_eq!(capture.object, capture.peeled_commit.clone().unwrap());
    let comparison = fixture
        .store
        .compare(&fixture.initial, &capture)
        .await
        .unwrap();
    assert!(comparison.fast_forward);
    assert_eq!(comparison.commits, 1);
    assert_eq!(comparison.files, 2);
    assert_eq!(comparison.insertions, 2);
    assert_eq!(comparison.deletions, 0);

    fs::write(pod.join("later.txt"), "later\n").unwrap();
    run_git(&fixture.git, &pod, &["add", "later.txt"]);
    run_git(&fixture.git, &pod, &["commit", "-m", "later change"]);
    let repeated_capture = fixture
        .store
        .import_capture(
            &Remote::new(path(&pod)).unwrap(),
            &WorkspaceId::new("workspace").unwrap(),
            &PodId::new("pod_branch").unwrap(),
            &SourceReference::new("refs/heads/main").unwrap(),
            &CaptureId::new("cap_branch").unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(repeated_capture.object, capture.object);

    let update = RefUpdate::new(
        capture.retained_as.clone(),
        ReferenceName::new("refs/heads/review/example").unwrap(),
        None,
        false,
    )
    .unwrap();
    let published = fixture
        .store
        .publish(&fixture.upstream_remote, std::slice::from_ref(&update))
        .await
        .unwrap();
    assert!(published.changed);
    assert!(!published.references[0].already_present);
    assert_eq!(
        object_id(&fixture.git, &fixture.upstream, "refs/heads/review/example"),
        capture.object
    );

    let retried = fixture
        .store
        .publish(&fixture.upstream_remote, &[update])
        .await
        .unwrap();
    assert!(!retried.changed);
    assert!(retried.references[0].already_present);
}

/// Verifies annotated tag objects survive capture and publication without
/// being replaced by their peeled commits.
#[tokio::test]
async fn preserves_and_publishes_annotated_tags() {
    let fixture = Fixture::new().await;
    let pod = fixture.clone_pod("tag");
    run_git(
        &fixture.git,
        &pod,
        &["tag", "-a", "v1.0.0", "-m", "release"],
    );
    let pod_tag = object_id(&fixture.git, &pod, "refs/tags/v1.0.0");
    let pod_commit = object_id(&fixture.git, &pod, "refs/tags/v1.0.0^{commit}");
    assert_ne!(pod_tag, pod_commit);

    let capture = fixture
        .store
        .import_capture(
            &Remote::new(path(&pod)).unwrap(),
            &WorkspaceId::new("workspace").unwrap(),
            &PodId::new("pod_tag").unwrap(),
            &SourceReference::new("refs/tags/v1.0.0").unwrap(),
            &CaptureId::new("cap_tag").unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(capture.kind, ObjectKind::Tag);
    assert_eq!(capture.object, pod_tag);
    assert_eq!(capture.peeled_commit, Some(pod_commit));

    let update = RefUpdate::new(
        capture.retained_as.clone(),
        ReferenceName::new("refs/tags/v1.0.0").unwrap(),
        None,
        false,
    )
    .unwrap();
    fixture
        .store
        .publish(&fixture.upstream_remote, &[update])
        .await
        .unwrap();
    assert_eq!(
        object_id(&fixture.git, &fixture.upstream, "refs/tags/v1.0.0"),
        capture.object
    );

    let advertised = upload_pack_advertisement(&fixture.store).await;
    assert!(!advertised.contains("refs/tascarrel/"));
    assert!(advertised.contains("refs/heads/main"));
    assert_eq!(
        fixture
            .store
            .remove_capture(
                &WorkspaceId::new("workspace").unwrap(),
                &PodId::new("pod_tag").unwrap(),
                &CaptureId::new("cap_tag").unwrap(),
            )
            .await
            .unwrap(),
        1
    );
    fixture.store.maintain().await.unwrap();
}

/// Verifies an existing tag cannot move until the exact rewrite receives
/// explicit authorization.
#[tokio::test]
async fn requires_explicit_approval_to_move_a_tag() {
    let fixture = Fixture::new().await;
    let pod = fixture.clone_pod("tag-move");
    run_git(
        &fixture.git,
        &pod,
        &["tag", "-a", "v1.0.0", "-m", "release"],
    );
    let initial = fixture
        .store
        .import_capture(
            &Remote::new(path(&pod)).unwrap(),
            &WorkspaceId::new("workspace").unwrap(),
            &PodId::new("pod_tag_move").unwrap(),
            &SourceReference::new("refs/tags/v1.0.0").unwrap(),
            &CaptureId::new("cap_tag_initial").unwrap(),
        )
        .await
        .unwrap();
    let create = RefUpdate::new(
        initial.retained_as,
        ReferenceName::new("refs/tags/v1.0.0").unwrap(),
        None,
        false,
    )
    .unwrap();
    fixture
        .store
        .publish(&fixture.upstream_remote, &[create])
        .await
        .unwrap();

    fs::write(pod.join("release.txt"), "next\n").unwrap();
    run_git(&fixture.git, &pod, &["add", "release.txt"]);
    run_git(&fixture.git, &pod, &["commit", "-m", "next release"]);
    run_git(
        &fixture.git,
        &pod,
        &["tag", "-f", "-a", "v1.0.0", "-m", "moved release"],
    );
    let moved = fixture
        .store
        .import_capture(
            &Remote::new(path(&pod)).unwrap(),
            &WorkspaceId::new("workspace").unwrap(),
            &PodId::new("pod_tag_move").unwrap(),
            &SourceReference::new("refs/tags/v1.0.0").unwrap(),
            &CaptureId::new("cap_tag_move").unwrap(),
        )
        .await
        .unwrap();
    let protected_move = RefUpdate::new(
        moved.retained_as.clone(),
        ReferenceName::new("refs/tags/v1.0.0").unwrap(),
        Some(initial.object.clone()),
        false,
    )
    .unwrap();
    let report = fixture
        .store
        .publish(&fixture.upstream_remote, &[protected_move])
        .await
        .expect_err("moving a tag requires rewrite approval");
    assert!(matches!(report.error(), GitError::TagExists { .. }));

    let approved_move = RefUpdate::new(
        moved.retained_as,
        ReferenceName::new("refs/tags/v1.0.0").unwrap(),
        Some(initial.object),
        true,
    )
    .unwrap();
    fixture
        .store
        .publish(&fixture.upstream_remote, &[approved_move])
        .await
        .unwrap();
    assert_eq!(
        object_id(&fixture.git, &fixture.upstream, "refs/tags/v1.0.0"),
        moved.object
    );
}

/// Verifies one approval can publish a branch and an annotated tag together
/// from capture refs in one workspace repository store.
#[tokio::test]
async fn publishes_multiple_refs_as_one_atomic_set() {
    let fixture = Fixture::new().await;
    let pod = fixture.clone_pod("atomic");
    fs::write(pod.join("release.txt"), "release\n").unwrap();
    run_git(&fixture.git, &pod, &["add", "release.txt"]);
    run_git(&fixture.git, &pod, &["commit", "-m", "prepare release"]);
    run_git(
        &fixture.git,
        &pod,
        &["tag", "-a", "v2.0.0", "-m", "release"],
    );

    let pod_remote = Remote::new(path(&pod)).unwrap();
    let branch = fixture
        .store
        .import_capture(
            &pod_remote,
            &WorkspaceId::new("workspace").unwrap(),
            &PodId::new("pod_atomic").unwrap(),
            &SourceReference::new("refs/heads/main").unwrap(),
            &CaptureId::new("cap_atomic").unwrap(),
        )
        .await
        .unwrap();
    let tag = fixture
        .store
        .import_capture(
            &pod_remote,
            &WorkspaceId::new("workspace").unwrap(),
            &PodId::new("pod_atomic").unwrap(),
            &SourceReference::new("refs/tags/v2.0.0").unwrap(),
            &CaptureId::new("cap_atomic").unwrap(),
        )
        .await
        .unwrap();
    let updates = [
        RefUpdate::new(
            branch.retained_as,
            ReferenceName::new("refs/heads/release/v2").unwrap(),
            None,
            false,
        )
        .unwrap(),
        RefUpdate::new(
            tag.retained_as,
            ReferenceName::new("refs/tags/v2.0.0").unwrap(),
            None,
            false,
        )
        .unwrap(),
    ];
    let published = fixture
        .store
        .publish(&fixture.upstream_remote, &updates)
        .await
        .unwrap();

    assert!(published.changed);
    assert_eq!(published.references.len(), 2);
    assert_eq!(
        object_id(&fixture.git, &fixture.upstream, "refs/heads/release/v2"),
        branch.object
    );
    assert_eq!(
        object_id(&fixture.git, &fixture.upstream, "refs/tags/v2.0.0"),
        tag.object
    );
}

/// Verifies receive-pack namespaces stage ref updates in one shared object
/// store and leave cached upstream refs unchanged until publication.
#[tokio::test]
async fn stages_receive_pack_updates_without_copying_the_repository() {
    let fixture = Fixture::new().await;
    let pod = fixture.clone_pod("receive-pack");
    fs::write(pod.join("pending.txt"), "pending approval\n").unwrap();
    run_git(&fixture.git, &pod, &["add", "pending.txt"]);
    run_git(&fixture.git, &pod, &["commit", "-m", "pending change"]);
    run_git(
        &fixture.git,
        &pod,
        &["tag", "-a", "v3.0.0", "-m", "pending release"],
    );
    let tag_object = object_id(&fixture.git, &pod, "refs/tags/v3.0.0");
    let capture = fixture
        .store
        .import_capture(
            &Remote::new(path(&pod)).unwrap(),
            &WorkspaceId::new("workspace").unwrap(),
            &PodId::new("pod_receive").unwrap(),
            &SourceReference::new("refs/heads/main").unwrap(),
            &CaptureId::new("cap_receive").unwrap(),
        )
        .await
        .unwrap();
    let namespace = ReceiveNamespace::new("approval_receive").unwrap();
    let baseline = fixture
        .store
        .stage_receive_namespace(&namespace)
        .await
        .unwrap();
    complete_receive_pack_without_updates(&fixture.store, &namespace).await;
    push_namespaced_refs(
        &fixture,
        &pod,
        &namespace,
        &["HEAD:refs/heads/main", "refs/tags/v3.0.0:refs/tags/v3.0.0"],
    );

    let updates = fixture
        .store
        .received_updates(&namespace, &baseline)
        .await
        .unwrap();
    assert_eq!(updates.len(), 2);
    let branch = updates
        .iter()
        .find(|update| update.destination.as_str() == "refs/heads/main")
        .unwrap();
    assert_eq!(branch.previous, Some(fixture.initial.clone()));
    assert_eq!(branch.proposed, capture.object);
    assert!(!branch.rewrites);
    let tag = updates
        .iter()
        .find(|update| update.destination.as_str() == "refs/tags/v3.0.0")
        .unwrap();
    assert_eq!(tag.previous, None);
    assert_eq!(tag.proposed, tag_object);
    assert!(!tag.rewrites);
    assert_eq!(
        object_id(&fixture.git, fixture.store.path(), "refs/heads/main"),
        fixture.initial
    );

    let publications = updates
        .iter()
        .map(|update| {
            RefUpdate::new(
                update.source.clone(),
                update.destination.clone(),
                update.previous.clone(),
                update.rewrites,
            )
            .unwrap()
        })
        .collect::<Vec<_>>();
    fixture
        .store
        .publish(&fixture.upstream_remote, &publications)
        .await
        .unwrap();
    assert_eq!(
        object_id(&fixture.git, &fixture.upstream, "refs/heads/main"),
        capture.object
    );
    assert_eq!(
        object_id(&fixture.git, &fixture.upstream, "refs/tags/v3.0.0"),
        tag_object
    );
    assert!(
        fixture
            .store
            .remove_receive_namespace(&namespace)
            .await
            .unwrap()
            > 0
    );
}

/// Verifies receive-pack accepts branch and tag destinations while rejecting
/// other Git namespaces before an approval can be staged.
#[tokio::test]
async fn receive_pack_rejects_unsupported_reference_names() {
    let fixture = Fixture::new().await;
    let hook = fixture.store.path().join("tascarrel-hooks/update");
    let old = fixture.initial.as_str();

    assert!(
        Command::new(&hook)
            .args(["refs/heads/main", old, old])
            .status()
            .unwrap()
            .success()
    );
    assert!(
        Command::new(&hook)
            .args(["refs/tags/v1.0.0", old, old])
            .status()
            .unwrap()
            .success()
    );
    assert!(
        !Command::new(&hook)
            .args(["refs/notes/review", old, old])
            .status()
            .unwrap()
            .success()
    );
}

/// Verifies identical capture names remain isolated by pod and pod cleanup
/// cannot remove another pod's retained refs.
#[tokio::test]
async fn isolates_capture_namespaces_and_reports_cache_statistics() {
    let fixture = Fixture::new().await;
    let pod = fixture.clone_pod("namespaces");
    let remote = Remote::new(path(&pod)).unwrap();
    let source = SourceReference::new("HEAD").unwrap();
    let capture_id = CaptureId::new("shared_capture").unwrap();
    let workspace_id = WorkspaceId::new("workspace").unwrap();
    let first_pod = PodId::new("pod_alpha").unwrap();
    let second_pod = PodId::new("pod_beta").unwrap();

    let first = fixture
        .store
        .import_capture(&remote, &workspace_id, &first_pod, &source, &capture_id)
        .await
        .unwrap();
    let second = fixture
        .store
        .import_capture(&remote, &workspace_id, &second_pod, &source, &capture_id)
        .await
        .unwrap();
    assert_ne!(first.retained_as, second.retained_as);
    assert!(first.retained_as.as_str().contains("/pod_alpha/captures/"));
    assert!(second.retained_as.as_str().contains("/pod_beta/captures/"));

    let statistics = fixture.store.statistics().await.unwrap();
    assert_eq!(statistics.branches, 1);
    assert_eq!(statistics.tags, 0);
    assert_eq!(statistics.captures, 2);
    assert!(statistics.loose_objects + statistics.packed_objects > 0);

    assert_eq!(
        fixture
            .store
            .remove_pod(&workspace_id, &first_pod)
            .await
            .unwrap(),
        1
    );
    assert!(!reference_exists(
        &fixture.git,
        fixture.store.path(),
        first.retained_as.as_str(),
    ));
    assert!(reference_exists(
        &fixture.git,
        fixture.store.path(),
        second.retained_as.as_str(),
    ));
}

/// Verifies a remote branch movement is reported as a lease conflict and is
/// never overwritten by the approved capture.
#[tokio::test]
async fn rejects_publication_after_the_upstream_lease_changes() {
    let fixture = Fixture::new().await;
    let pod = fixture.clone_pod("conflict");
    fs::write(pod.join("pod.txt"), "pod\n").unwrap();
    run_git(&fixture.git, &pod, &["add", "pod.txt"]);
    run_git(&fixture.git, &pod, &["commit", "-m", "pod change"]);
    let capture = fixture
        .store
        .import_capture(
            &Remote::new(path(&pod)).unwrap(),
            &WorkspaceId::new("workspace").unwrap(),
            &PodId::new("pod_conflict").unwrap(),
            &SourceReference::new("refs/heads/main").unwrap(),
            &CaptureId::new("cap_conflict").unwrap(),
        )
        .await
        .unwrap();

    fs::write(fixture.source.join("upstream.txt"), "upstream\n").unwrap();
    run_git(&fixture.git, &fixture.source, &["add", "upstream.txt"]);
    run_git(
        &fixture.git,
        &fixture.source,
        &["commit", "-m", "upstream change"],
    );
    run_git(&fixture.git, &fixture.source, &["push", "origin", "main"]);
    let moved = object_id(&fixture.git, &fixture.source, "HEAD");

    let update = RefUpdate::new(
        capture.retained_as,
        ReferenceName::new("refs/heads/main").unwrap(),
        Some(fixture.initial),
        false,
    )
    .unwrap();
    let report = fixture
        .store
        .publish(&fixture.upstream_remote, &[update])
        .await
        .expect_err("changed lease must fail");
    assert!(matches!(
        report.error(),
        GitError::LeaseConflict { actual: Some(actual), .. } if actual == moved.as_str()
    ));
    assert_eq!(
        object_id(&fixture.git, &fixture.upstream, "refs/heads/main"),
        moved
    );
}

fn isolated_git() -> GitBinary {
    GitBinary::discover()
        .unwrap()
        .with_environment("GIT_CONFIG_NOSYSTEM", "1")
        .with_environment("GIT_CONFIG_GLOBAL", "/dev/null")
        .with_environment("GIT_TERMINAL_PROMPT", "0")
        .without_environment("GIT_DIR")
        .without_environment("GIT_WORK_TREE")
        .without_environment("GIT_INDEX_FILE")
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn push_namespaced_refs(
    fixture: &Fixture,
    pod: &Path,
    namespace: &ReceiveNamespace,
    refspecs: &[&str],
) {
    let receive_pack = format!(
        "env GIT_NAMESPACE={} {} -c receive.denyDeletes=true receive-pack",
        namespace.as_str(),
        shell_quote(fixture.git.executable().to_string_lossy().as_ref()),
    );
    let output = test_command(&fixture.git, pod, &[])
        .arg("push")
        .arg(format!("--receive-pack={receive_pack}"))
        .arg(fixture.store.path())
        .args(refspecs)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "namespaced Git push failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn run_git(git: &GitBinary, directory: &Path, arguments: &[&str]) {
    let output = test_command(git, directory, arguments).output().unwrap();
    assert!(
        output.status.success(),
        "Git {:?} failed: {}",
        arguments,
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git_output(git: &GitBinary, directory: &Path, arguments: &[&str]) -> String {
    let output = test_command(git, directory, arguments).output().unwrap();
    assert!(
        output.status.success(),
        "Git {:?} failed: {}",
        arguments,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn object_id(git: &GitBinary, directory: &Path, reference: &str) -> ObjectId {
    let output = git_output(git, directory, &["rev-parse", "--verify", reference]);
    ObjectId::new(output.trim()).unwrap()
}

fn reference_exists(git: &GitBinary, directory: &Path, reference: &str) -> bool {
    test_command(
        git,
        directory,
        &["show-ref", "--verify", "--quiet", reference],
    )
    .status()
    .unwrap()
    .success()
}

async fn upload_pack_advertisement(store: &RepositoryStore) -> String {
    let service = store.upload_pack().unwrap();
    let (mut client, server) = tokio::io::duplex(1024 * 1024);
    let relay = tokio::spawn(service.relay(server));
    client.write_all(b"0000").await.unwrap();
    client.shutdown().await.unwrap();
    let mut advertisement = Vec::new();
    client.read_to_end(&mut advertisement).await.unwrap();
    relay.await.unwrap().unwrap();
    String::from_utf8_lossy(&advertisement).into_owned()
}

async fn complete_receive_pack_without_updates(
    store: &RepositoryStore,
    namespace: &ReceiveNamespace,
) {
    let service = store.receive_pack(namespace).unwrap();
    let (mut client, server) = tokio::io::duplex(1024 * 1024);
    let relay = tokio::spawn(service.relay_retained(server));
    loop {
        let mut header = [0_u8; 4];
        client.read_exact(&mut header).await.unwrap();
        if &header == b"0000" {
            break;
        }
        let length = usize::from_str_radix(std::str::from_utf8(&header).unwrap(), 16).unwrap();
        let mut payload = vec![0_u8; length - header.len()];
        client.read_exact(&mut payload).await.unwrap();
    }
    client.write_all(b"0000").await.unwrap();
    client.shutdown().await.unwrap();
    let retained = tokio::time::timeout(std::time::Duration::from_secs(5), relay)
        .await
        .expect("receive-pack relay did not finish")
        .expect("receive-pack relay task failed")
        .expect("receive-pack rejected an empty update set");
    drop(retained);
    let mut remainder = Vec::new();
    client.read_to_end(&mut remainder).await.unwrap();
}

fn test_command(git: &GitBinary, directory: &Path, arguments: &[&str]) -> Command {
    let mut command = Command::new(git.executable());
    command
        .current_dir(directory)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .args(arguments);
    command
}

fn path(path: &Path) -> &str {
    path.to_str().unwrap()
}
