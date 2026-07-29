//! Typed failures produced by managed Git operations.

use std::io;
use std::path::PathBuf;

use reportify::Report;
use thiserror::Error;

/// Failure while validating or executing a managed Git operation.
#[derive(Debug, Error)]
pub enum GitError {
    /// No usable Git executable could be discovered.
    #[error("failed to discover a Git executable")]
    GitNotFound,
    /// A configured Git executable is not an absolute regular file.
    #[error("invalid Git executable path {path}")]
    InvalidExecutable {
        /// Invalid executable path.
        path: PathBuf,
    },
    /// A managed repository path is invalid.
    #[error("invalid Git repository path {path}")]
    InvalidRepositoryPath {
        /// Invalid repository path.
        path: PathBuf,
    },
    /// An object ID is malformed or uses an unsupported hash length.
    #[error("invalid Git object ID")]
    InvalidObjectId,
    /// A full Git reference name is malformed.
    #[error("invalid Git reference name {reference:?}")]
    InvalidReference {
        /// Invalid reference name.
        reference: String,
    },
    /// A capture identifier cannot safely form part of an internal ref.
    #[error("invalid Git capture identifier {capture_id:?}")]
    InvalidCaptureId {
        /// Invalid capture identifier.
        capture_id: String,
    },
    /// An approval identifier cannot safely form part of an internal ref.
    #[error("invalid Git approval identifier {approval_id:?}")]
    InvalidApprovalId {
        /// Invalid approval identifier.
        approval_id: String,
    },
    /// A receive-pack namespace cannot safely form part of an internal ref.
    #[error("invalid Git receive-pack namespace {namespace:?}")]
    InvalidReceiveNamespace {
        /// Invalid receive-pack namespace.
        namespace: String,
    },
    /// A pod identifier cannot safely form part of an internal ref.
    #[error("invalid Git pod identifier {pod_id:?}")]
    InvalidPodId {
        /// Invalid pod identifier.
        pod_id: String,
    },
    /// A workspace identifier cannot safely form part of an internal ref.
    #[error("invalid Git workspace identifier {workspace_id:?}")]
    InvalidWorkspaceId {
        /// Invalid workspace identifier.
        workspace_id: String,
    },
    /// A capture source is neither `HEAD` nor a supported full ref.
    #[error("invalid Git source reference {reference:?}")]
    InvalidSourceReference {
        /// Invalid source reference.
        reference: String,
    },
    /// A remote location cannot safely be passed to Git.
    #[error("invalid Git remote location")]
    InvalidRemote,
    /// Configured structured-output limits contain a zero bound.
    #[error("invalid Git resource limits")]
    InvalidLimits,
    /// A publication destination is outside the supported branch and tag
    /// namespaces.
    #[error("unsupported Git publication destination {reference}")]
    UnsupportedDestination {
        /// Unsupported destination reference.
        reference: String,
    },
    /// A required reference is absent from the managed repository.
    #[error("Git reference {reference} does not exist")]
    MissingReference {
        /// Missing reference name.
        reference: String,
    },
    /// A branch or tag operation requires an object that peels to a commit.
    #[error("Git reference {reference} does not resolve to a commit")]
    NotCommit {
        /// Reference whose object cannot be peeled to a commit.
        reference: String,
    },
    /// A publication lease no longer matches the upstream reference.
    #[error("Git reference {reference} changed upstream")]
    LeaseConflict {
        /// Destination reference.
        reference: String,
        /// Approved upstream value, or absence for a new ref.
        expected: Option<String>,
        /// Current upstream value, or absence when the ref was removed.
        actual: Option<String>,
    },
    /// A branch update is not a fast-forward and was not authorized as a
    /// rewrite.
    #[error("Git branch update for {reference} is not a fast-forward")]
    NonFastForward {
        /// Destination branch.
        reference: String,
    },
    /// An existing tag would be moved without explicit rewrite authorization.
    #[error("Git tag {reference} already exists")]
    TagExists {
        /// Destination tag.
        reference: String,
    },
    /// Git produced more structured output than the configured bound.
    #[error("Git {action} output exceeded the configured limit of {limit} bytes")]
    OutputLimit {
        /// Operation producing the output.
        action: &'static str,
        /// Configured byte limit.
        limit: usize,
    },
    /// Git returned more refs than the configured bound.
    #[error("Git {action} returned more than {limit} references")]
    ReferenceLimit {
        /// Operation producing the references.
        action: &'static str,
        /// Configured reference-count limit.
        limit: usize,
    },
    /// Git produced structured output that could not be decoded or validated.
    #[error("Git {action} produced invalid output")]
    MalformedOutput {
        /// Operation producing the invalid output.
        action: &'static str,
    },
    /// A Git subprocess rejected or failed an operation.
    #[error("Git {action} failed: {diagnostic}")]
    Command {
        /// Operation being performed.
        action: &'static str,
        /// Process exit code when Git exited normally.
        status: Option<i32>,
        /// Bounded, redacted diagnostic.
        diagnostic: String,
    },
    /// A filesystem or subprocess operation failed.
    #[error("failed to {action}")]
    Io {
        /// Operation that failed.
        action: &'static str,
        /// Underlying I/O failure.
        #[source]
        source: io::Error,
    },
    /// A repository exists but is not a usable managed bare repository.
    #[error("invalid managed bare Git repository {path}")]
    InvalidRepository {
        /// Repository path.
        path: PathBuf,
    },
    /// A publication contains no ref updates or repeats a destination.
    #[error("invalid Git publication: {reason}")]
    InvalidPublication {
        /// Safe explanation of the contract violation.
        reason: &'static str,
    },
    /// Git requested a service that the configured broker does not expose.
    #[error("unsupported Git remote service {service}")]
    UnsupportedService {
        /// Requested Git service name.
        service: String,
    },
}

/// Result returned by managed Git operations.
pub type GitResult<T> = Result<T, Report<GitError>>;
