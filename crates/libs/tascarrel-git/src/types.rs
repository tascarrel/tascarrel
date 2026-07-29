//! Validated Git identifiers and operation records.

use std::fmt;

use reportify::Report;

use crate::GitError;
use crate::GitResult;

/// Full hexadecimal Git object identifier.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ObjectId(String);

impl ObjectId {
    /// Parses a SHA-1 or SHA-256 object identifier.
    ///
    /// # Errors
    ///
    /// Returns [`GitError::InvalidObjectId`] for another length or a
    /// non-hexadecimal value.
    pub fn new(value: impl Into<String>) -> GitResult<Self> {
        let value = value.into();
        if !matches!(value.len(), 40 | 64) || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(Report::new(GitError::InvalidObjectId));
        }
        Ok(Self(value.to_ascii_lowercase()))
    }

    /// Returns the lowercase hexadecimal object identifier.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ObjectId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Validated full Git reference beginning with `refs/`.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ReferenceName(String);

impl ReferenceName {
    /// Validates a full Git reference name.
    ///
    /// # Errors
    ///
    /// Returns [`GitError::InvalidReference`] when Git would reject the name.
    pub fn new(value: impl Into<String>) -> GitResult<Self> {
        let value = value.into();
        if !valid_reference(&value) {
            return Err(Report::new(GitError::InvalidReference { reference: value }));
        }
        Ok(Self(value))
    }

    /// Returns the full reference name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns whether this reference names a branch.
    #[must_use]
    pub fn is_branch(&self) -> bool {
        self.0.starts_with("refs/heads/")
    }

    /// Returns whether this reference names a tag.
    #[must_use]
    pub fn is_tag(&self) -> bool {
        self.0.starts_with("refs/tags/")
    }
}

impl fmt::Display for ReferenceName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Opaque identifier used to retain a captured set of refs.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CaptureId(String);

impl CaptureId {
    /// Validates an opaque capture identifier for use in internal refs.
    ///
    /// # Errors
    ///
    /// Returns [`GitError::InvalidCaptureId`] when the identifier is empty,
    /// too long, or contains characters outside ASCII letters, digits, `_`,
    /// and `-`.
    pub fn new(value: impl Into<String>) -> GitResult<Self> {
        const MAX_CAPTURE_ID_BYTES: usize = 128;

        let value = value.into();
        if value.is_empty()
            || value.len() > MAX_CAPTURE_ID_BYTES
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        {
            return Err(Report::new(GitError::InvalidCaptureId {
                capture_id: value,
            }));
        }
        Ok(Self(value))
    }

    /// Returns the opaque capture identifier.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CaptureId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Opaque identifier used to retain objects required for approval review.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ApprovalId(String);

impl ApprovalId {
    /// Validates an approval identifier for use in internal refs.
    ///
    /// # Errors
    ///
    /// Returns [`GitError::InvalidApprovalId`] when the identifier is empty,
    /// too long, or contains characters outside ASCII letters, digits, `_`,
    /// and `-`.
    pub fn new(value: impl Into<String>) -> GitResult<Self> {
        const MAX_APPROVAL_ID_BYTES: usize = 128;

        let value = value.into();
        if value.is_empty()
            || value.len() > MAX_APPROVAL_ID_BYTES
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        {
            return Err(Report::new(GitError::InvalidApprovalId {
                approval_id: value,
            }));
        }
        Ok(Self(value))
    }

    /// Returns the opaque approval identifier.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ApprovalId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Opaque Git namespace used for one receive-pack staging operation.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ReceiveNamespace(String);

impl ReceiveNamespace {
    /// Validates a receive-pack namespace identifier.
    ///
    /// # Errors
    ///
    /// Returns [`GitError::InvalidReceiveNamespace`] when the identifier cannot
    /// be represented as one safe Git namespace component.
    pub fn new(value: impl Into<String>) -> GitResult<Self> {
        const MAX_NAMESPACE_BYTES: usize = 128;

        let value = value.into();
        if value.is_empty()
            || value.len() > MAX_NAMESPACE_BYTES
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        {
            return Err(Report::new(GitError::InvalidReceiveNamespace {
                namespace: value,
            }));
        }
        Ok(Self(value))
    }

    /// Returns the namespace component.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ReceiveNamespace {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Opaque pod identifier used to isolate captured refs.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PodId(String);

impl PodId {
    /// Validates a pod identifier for use in internal refs.
    ///
    /// # Errors
    ///
    /// Returns [`GitError::InvalidPodId`] when the identifier is empty, too
    /// long, or contains characters outside ASCII letters, digits, `_`, and
    /// `-`.
    pub fn new(value: impl Into<String>) -> GitResult<Self> {
        const MAX_POD_ID_BYTES: usize = 128;

        let value = value.into();
        if value.is_empty()
            || value.len() > MAX_POD_ID_BYTES
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        {
            return Err(Report::new(GitError::InvalidPodId { pod_id: value }));
        }
        Ok(Self(value))
    }

    /// Returns the opaque pod identifier.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PodId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Opaque workspace identifier used to group pod capture namespaces.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct WorkspaceId(String);

impl WorkspaceId {
    /// Validates a workspace identifier for use in internal refs.
    ///
    /// # Errors
    ///
    /// Returns [`GitError::InvalidWorkspaceId`] when the identifier is empty,
    /// too long, or contains characters outside ASCII letters, digits, `_`,
    /// and `-`.
    pub fn new(value: impl Into<String>) -> GitResult<Self> {
        const MAX_WORKSPACE_ID_BYTES: usize = 128;

        let value = value.into();
        if value.is_empty()
            || value.len() > MAX_WORKSPACE_ID_BYTES
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        {
            return Err(Report::new(GitError::InvalidWorkspaceId {
                workspace_id: value,
            }));
        }
        Ok(Self(value))
    }

    /// Returns the opaque workspace identifier.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for WorkspaceId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Validated source selected from another Git repository.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SourceReference(String);

impl SourceReference {
    /// Validates `HEAD` or a full branch or tag reference.
    ///
    /// # Errors
    ///
    /// Returns [`GitError::InvalidSourceReference`] for another symbolic name
    /// or reference namespace.
    pub fn new(value: impl Into<String>) -> GitResult<Self> {
        let value = value.into();
        if value == "HEAD" {
            return Ok(Self(value));
        }
        if !valid_reference(&value)
            || !(value.starts_with("refs/heads/") || value.starts_with("refs/tags/"))
        {
            return Err(Report::new(GitError::InvalidSourceReference {
                reference: value,
            }));
        }
        Ok(Self(value))
    }

    /// Returns `HEAD` or the full source reference.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SourceReference {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Git remote location passed only to a managed Git subprocess.
#[derive(Clone, Eq, PartialEq)]
pub struct Remote(String);

impl Remote {
    /// Validates a remote URL or local repository path.
    ///
    /// # Errors
    ///
    /// Returns [`GitError::InvalidRemote`] for an empty value, an option-like
    /// value, or control characters that cannot safely form a Git argument.
    pub fn new(value: impl Into<String>) -> GitResult<Self> {
        let value = value.into();
        if value.is_empty()
            || value.starts_with('-')
            || value
                .bytes()
                .any(|byte| byte == 0 || byte == b'\n' || byte == b'\r')
        {
            return Err(Report::new(GitError::InvalidRemote));
        }
        Ok(Self(value))
    }

    /// Returns the remote argument.
    ///
    /// Callers must avoid logging remote URLs because they may contain user
    /// names or credential material.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Remote {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Remote(<redacted>)")
    }
}

/// Kind of object stored at a Git reference.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ObjectKind {
    /// Commit object.
    Commit,
    /// Annotated tag object.
    Tag,
    /// Tree object.
    Tree,
    /// Blob object.
    Blob,
}

/// One upstream branch or tag retained by a managed repository.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryReference {
    /// Full reference name.
    pub name: ReferenceName,
    /// Exact object stored at the reference.
    pub object: ObjectId,
    /// Object kind before peeling annotated tags.
    pub kind: ObjectKind,
    /// Commit reached by peeling the object, when one exists.
    pub peeled_commit: Option<ObjectId>,
}

/// Complete tracked upstream state observed by one successful refresh.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryRefresh {
    /// Branch selected by the upstream symbolic `HEAD`, when advertised.
    pub default_branch: Option<ReferenceName>,
    /// Upstream branches and tags retained by the managed repository.
    pub references: Vec<RepositoryReference>,
}

/// One pod or guest ref imported into the hidden capture namespace.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapturedReference {
    /// Workspace containing the pod.
    pub workspace_id: WorkspaceId,
    /// Pod whose repository supplied the object.
    pub pod_id: PodId,
    /// Capture retaining the object.
    pub capture_id: CaptureId,
    /// Logical source ref supplied by the guest repository.
    pub source: SourceReference,
    /// Hidden ref created in the managed object store.
    pub retained_as: ReferenceName,
    /// Exact object stored at the source ref.
    pub object: ObjectId,
    /// Object kind before peeling annotated tags.
    pub kind: ObjectKind,
    /// Commit reached by peeling the object, when one exists.
    pub peeled_commit: Option<ObjectId>,
}

/// One branch or tag changed by a namespaced receive-pack operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReceivedReferenceUpdate {
    /// Retained namespaced ref containing the proposed object.
    pub source: ReferenceName,
    /// Upstream branch or tag proposed by the Git client.
    pub destination: ReferenceName,
    /// Object advertised before receive-pack, when the ref already existed.
    pub previous: Option<ObjectId>,
    /// Exact object retained after receive-pack.
    pub proposed: ObjectId,
    /// Whether the update rewrites branch history or replaces an existing tag.
    pub rewrites: bool,
}

/// Comparison between an upstream base and one captured commit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReferenceComparison {
    /// Whether the captured commit descends from the base.
    pub fast_forward: bool,
    /// Commits reachable from the capture but not from the base.
    pub commits: u64,
    /// Changed paths in the comparison.
    pub files: u64,
    /// Text-line insertions reported by Git.
    pub insertions: u64,
    /// Text-line deletions reported by Git.
    pub deletions: u64,
    /// Files for which Git reported binary changes.
    pub binary_files: u64,
}

/// Metadata recorded for one Git commit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitCommit {
    /// Exact commit object identifier.
    pub id: ObjectId,
    /// Parent commit identifiers in recorded order.
    pub parents: Vec<ObjectId>,
    /// Author identity and timestamp recorded by the commit.
    pub author: GitSignature,
    /// Committer identity and timestamp recorded by the commit.
    pub committer: GitSignature,
    /// First line of the commit message.
    pub subject: String,
    /// Remaining commit message after the subject.
    pub body: String,
}

/// Identity and ISO 8601 timestamp recorded in a Git commit signature.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitSignature {
    /// Display name recorded by Git.
    pub name: String,
    /// Email address recorded by Git.
    pub email: String,
    /// Absolute timestamp emitted by Git's strict ISO 8601 formatter.
    pub timestamp: String,
}

/// Approved update from a retained cache ref to an upstream branch or tag.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RefUpdate {
    source: ReferenceName,
    destination: ReferenceName,
    expected: Option<ObjectId>,
    allow_rewrite: bool,
}

impl RefUpdate {
    /// Creates one lease-protected branch or tag update.
    ///
    /// `expected` is the approved upstream object, or `None` when the ref must
    /// not yet exist.
    ///
    /// # Errors
    ///
    /// Returns [`GitError::UnsupportedDestination`] for another ref namespace.
    pub fn new(
        source: ReferenceName,
        destination: ReferenceName,
        expected: Option<ObjectId>,
        allow_rewrite: bool,
    ) -> GitResult<Self> {
        if !destination.is_branch() && !destination.is_tag() {
            return Err(Report::new(GitError::UnsupportedDestination {
                reference: destination.to_string(),
            }));
        }
        Ok(Self {
            source,
            destination,
            expected,
            allow_rewrite,
        })
    }

    /// Returns the retained source ref.
    #[must_use]
    pub fn source(&self) -> &ReferenceName {
        &self.source
    }

    /// Returns the upstream destination ref.
    #[must_use]
    pub fn destination(&self) -> &ReferenceName {
        &self.destination
    }

    /// Returns the approved upstream value, or absence for ref creation.
    #[must_use]
    pub fn expected(&self) -> Option<&ObjectId> {
        self.expected.as_ref()
    }

    /// Returns whether a branch or tag rewrite was explicitly approved.
    #[must_use]
    pub fn allows_rewrite(&self) -> bool {
        self.allow_rewrite
    }
}

/// Result of publishing one approved reference.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublishedReference {
    /// Updated upstream reference.
    pub reference: ReferenceName,
    /// Exact object now stored upstream.
    pub object: ObjectId,
    /// Whether the upstream already had the approved value before this attempt.
    pub already_present: bool,
}

/// Result of one atomic publication attempt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublishOutcome {
    /// Complete approved ref set and its resulting upstream values.
    pub references: Vec<PublishedReference>,
    /// Whether this attempt changed at least one upstream ref.
    pub changed: bool,
}

/// Resource bounds applied while parsing Git subprocess output.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitLimits {
    /// Maximum retained stderr bytes for a failed Git command.
    pub diagnostic_bytes: usize,
    /// Maximum stdout bytes accepted from structured Git queries.
    pub command_output_bytes: usize,
    /// Maximum refs accepted from one Git query.
    pub references: usize,
}

/// Storage and reference counts reported for one managed repository.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryStatistics {
    /// Cached upstream branch refs.
    pub branches: usize,
    /// Cached upstream tag refs.
    pub tags: usize,
    /// Hidden immutable capture refs across every pod.
    pub captures: usize,
    /// Loose objects reported by Git.
    pub loose_objects: u64,
    /// Objects retained in pack files.
    pub packed_objects: u64,
    /// Pack files retained by Git.
    pub packs: u64,
    /// Approximate bytes occupied by loose objects, packs, and garbage.
    pub size_bytes: u64,
    /// Bytes Git classifies as garbage.
    pub garbage_bytes: u64,
}

impl Default for GitLimits {
    fn default() -> Self {
        Self {
            diagnostic_bytes: 64 * 1024,
            command_output_bytes: 16 * 1024 * 1024,
            references: 100_000,
        }
    }
}

fn valid_reference(value: &str) -> bool {
    if !value.starts_with("refs/")
        || value.ends_with('/')
        || value.ends_with('.')
        || value == "refs/@"
        || value.contains("..")
        || value.contains("@{")
        || value.contains("//")
        || value
            .bytes()
            .any(|byte| byte <= b' ' || byte == 0x7f || b"~^:?*[\\".contains(&byte))
    {
        return false;
    }

    value.split('/').all(|component| {
        !component.is_empty()
            && !component.starts_with('.')
            && !component
                .as_bytes()
                .get(component.len().saturating_sub(5)..)
                .is_some_and(|suffix| suffix.eq_ignore_ascii_case(b".lock"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verifies reference validation accepts ordinary and internal refs while
    /// rejecting unsafe forms.
    #[test]
    fn validates_full_reference_names() {
        for valid in [
            "refs/heads/main",
            "refs/tags/v1.2.3",
            "refs/tascarrel/workspaces/demo/pods/pod_123/captures/cap_123/refs/heads/topic",
        ] {
            assert_eq!(ReferenceName::new(valid).unwrap().as_str(), valid);
        }
        for invalid in [
            "main",
            "refs/heads/../main",
            "refs/heads/.hidden",
            "refs/heads/main.lock",
            "refs/heads/a b",
            "refs/heads/a~b",
        ] {
            assert!(ReferenceName::new(invalid).is_err(), "accepted {invalid}");
        }
    }

    /// Verifies object IDs retain exact SHA-1 and SHA-256 values in normalized
    /// form.
    #[test]
    fn validates_supported_object_ids() {
        let sha1 = "A".repeat(40);
        let sha256 = "b".repeat(64);
        assert_eq!(ObjectId::new(sha1).unwrap().as_str(), "a".repeat(40));
        assert_eq!(ObjectId::new(&sha256).unwrap().as_str(), sha256);
        assert!(ObjectId::new("abc").is_err());
    }
}
