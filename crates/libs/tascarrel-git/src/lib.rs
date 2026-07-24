//! Managed Git mechanics shared by Tascarrel host and guest services.
//!
//! [`RepositoryStore`] owns one bare object database containing cached
//! upstream branches and tags plus hidden, capture-scoped refs. It refreshes
//! upstream state, imports exact refs from another Git service, compares
//! captured commits, publishes approved ref sets with explicit leases, and
//! performs incremental maintenance. [`GitService`] exposes upload-pack and
//! receive-pack over any asynchronous full-duplex byte stream.
//!
//! [`GitBinary`] deliberately invokes the system Git executable. A host-side
//! caller can therefore reuse the user's credential helpers, SSH agent, proxy,
//! and certificate configuration without copying credentials into a workspace
//! VM. The crate never decides which repository or operation a principal may
//! access; hostd and guestd remain responsible for identity, authorization,
//! approval, durable domain records, and transport routing.
//!
//! [`run_remote_helper`] implements Git's blocking `connect` helper handshake
//! against a caller-provided service connector. It contains no Tascarrel wire
//! or daemon dependency.

#![deny(unsafe_code)]

mod command;
mod error;
mod remote_helper;
mod store;
mod transport;
mod types;

pub use command::GitBinary;
pub use error::GitError;
pub use error::GitResult;
pub use remote_helper::RemoteService;
pub use remote_helper::run_remote_helper;
pub use store::RepositoryStore;
pub use transport::GitService;
pub use types::CaptureId;
pub use types::CapturedReference;
pub use types::GitLimits;
pub use types::ObjectId;
pub use types::ObjectKind;
pub use types::PodId;
pub use types::PublishOutcome;
pub use types::PublishedReference;
pub use types::ReceiveNamespace;
pub use types::ReceivedReferenceUpdate;
pub use types::RefUpdate;
pub use types::ReferenceComparison;
pub use types::ReferenceName;
pub use types::Remote;
pub use types::RepositoryReference;
pub use types::RepositoryRefresh;
pub use types::RepositoryStatistics;
pub use types::SourceReference;
pub use types::WorkspaceId;
