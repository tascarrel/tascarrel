//! Host application of exact, guest-snapshotted `ShareFS` revisions.
//!
//! The service validates every untrusted guest path and content value, checks
//! all captured lower leases before mutation, and anchors traversal to the
//! host directory with no-follow directory file descriptors.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::ffi::OsString;
use std::fs::File;
use std::io::Read as _;
use std::io::Write as _;
use std::os::fd::AsFd as _;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::ffi::OsStringExt as _;
use std::path::PathBuf;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use reportify::ErrorExt as _;
use reportify::Report;
use rustix::fs::AtFlags;
use rustix::fs::FileType;
use rustix::fs::Mode;
use rustix::fs::OFlags;
use rustix::fs::Stat;
use sha2::Digest as _;
use sha2::Sha256;
use similar::TextDiff;
use tascarrel_api::types::shares as api;
use tascarrel_mux::Channel;
use tascarrel_protocol::Framed;
use tascarrel_protocol::MAX_SHARE_OVERLAY_CHANGES;
use tascarrel_protocol::MAX_SHARE_OVERLAY_CONTENT_BYTES;
use tascarrel_protocol::MAX_SHARE_OVERLAY_FRAME_LEN;
use tascarrel_protocol::MUX_SHARE_OVERLAY_GUEST_ENDPOINT;
use tascarrel_protocol::ShareOverlayBase;
use tascarrel_protocol::ShareOverlayChange;
use tascarrel_protocol::ShareOverlayCompletion;
use tascarrel_protocol::ShareOverlayDecision;
use tascarrel_protocol::ShareOverlayEntryKind;
use tascarrel_protocol::ShareOverlayEntryVersion;
use tascarrel_protocol::ShareOverlayOperation;
use tascarrel_protocol::ShareOverlayPrepareResponse;
use tascarrel_protocol::ShareOverlayRequest;
use tascarrel_protocol::ShareOverlaySnapshot;
use thiserror::Error;

use super::workspaces::WorkspaceOverlayShare;
use super::workspaces::WorkspaceService;

const MAX_PATH_BYTES: usize = 4096;
const MAX_TEXT_DIFF_BYTES: usize = 256 * 1024;

/// Applies no mutation and returns one exact revision for explicit review.
#[tracing::instrument(
    level = "debug",
    skip(workspaces, input),
    fields(
        workspace = %input.workspace,
        pod_id = %input.pod_id.0,
        share = %input.share
    ),
    err
)]
pub(crate) async fn inspect(
    workspaces: &WorkspaceService,
    input: api::InspectShareOverlayAction,
) -> Result<api::InspectShareOverlayOutput, Report<ShareOverlayServiceError>> {
    let connection = connection(workspaces, &input.workspace, input.share.as_ref()).await?;
    let snapshot = prepare(
        connection
            .mux
            .open(MUX_SHARE_OVERLAY_GUEST_ENDPOINT)
            .await
            .map_err(|error| unavailable(error.to_string()))?,
        ShareOverlayRequest {
            pod_id: input.pod_id.0.to_string(),
            share: input.share.to_string(),
            operation: ShareOverlayOperation::Inspect,
        },
    )
    .await?;
    let validated = validate_snapshot(snapshot)?;
    Ok(api::InspectShareOverlayOutput {
        revision: api::ShareOverlayRevision::new(validated.revision.clone()),
        changes: summaries(&validated),
    })
}

/// Validates and applies one exact reviewed revision.
#[tracing::instrument(
    level = "debug",
    skip(workspaces, input),
    fields(
        workspace = %input.workspace,
        pod_id = %input.pod_id.0,
        share = %input.share,
        revision = %input.revision
    ),
    err
)]
pub(crate) async fn apply(
    workspaces: &WorkspaceService,
    input: api::ApplyShareOverlayAction,
) -> Result<api::ApplyShareOverlayOutput, Report<ShareOverlayServiceError>> {
    let connection = connection(workspaces, &input.workspace, input.share.as_ref()).await?;
    let channel = connection
        .mux
        .open(MUX_SHARE_OVERLAY_GUEST_ENDPOINT)
        .await
        .map_err(|error| unavailable(error.to_string()))?;
    let mut framed = Framed::with_max_frame_len(channel, MAX_SHARE_OVERLAY_FRAME_LEN)
        .map_err(|error| unavailable(error.to_string()))?;
    framed
        .write(&ShareOverlayRequest {
            pod_id: input.pod_id.0.to_string(),
            share: input.share.to_string(),
            operation: ShareOverlayOperation::Apply {
                revision: input.revision.as_str().to_owned(),
            },
        })
        .await
        .map_err(|error| unavailable(error.to_string()))?;
    let response = framed
        .read::<ShareOverlayPrepareResponse>()
        .await
        .map_err(|error| unavailable(error.to_string()))?
        .ok_or_else(|| unavailable("guest closed the ShareFS approval request"))?;
    let snapshot = match response {
        ShareOverlayPrepareResponse::RevisionChanged { snapshot } => {
            let validated = validate_snapshot(snapshot)?;
            return Ok(api::ApplyShareOverlayOutput {
                result: api::ShareOverlayApplyResult::RevisionChanged(
                    api::ShareOverlayRevisionChanged {
                        revision: api::ShareOverlayRevision::new(validated.revision.clone()),
                        changes: summaries(&validated),
                    },
                ),
            });
        }
        ShareOverlayPrepareResponse::Snapshot { snapshot } => validate_snapshot(snapshot)?,
        ShareOverlayPrepareResponse::Error { error } => {
            return Err(unavailable(format!(
                "guest could not prepare the ShareFS revision: {}",
                error.message
            )));
        }
    };
    if snapshot.revision != input.revision.as_str() {
        send_decision(&mut framed, ShareOverlayDecision::Retain).await?;
        return Err(invalid("guest returned a different ShareFS revision"));
    }

    let root = connection.host_root;
    let attempted = tokio::task::spawn_blocking(move || validate_and_apply(&root, &snapshot))
        .await
        .map_err(|error| internal(format!("ShareFS approval task failed: {error}")))?;
    match attempted {
        Ok(ApplyAttempt::Applied) => {
            send_decision(&mut framed, ShareOverlayDecision::Applied).await?;
            Ok(api::ApplyShareOverlayOutput {
                result: api::ShareOverlayApplyResult::Applied,
            })
        }
        Ok(ApplyAttempt::Conflicts(conflicts)) => {
            send_decision(&mut framed, ShareOverlayDecision::Retain).await?;
            Ok(api::ApplyShareOverlayOutput {
                result: api::ShareOverlayApplyResult::Conflicts(api::ShareOverlayConflictList {
                    conflicts: conflicts.into(),
                }),
            })
        }
        Err(error) => {
            send_decision(&mut framed, ShareOverlayDecision::Retain).await?;
            Err(error)
        }
    }
}

/// Host-side overlay approval failure.
#[derive(Debug, Error)]
pub(crate) enum ShareOverlayServiceError {
    /// Request or untrusted guest data violated the contract.
    #[error("invalid share overlay request: {0}")]
    InvalidRequest(String),
    /// The selected workspace or guest endpoint is unavailable.
    #[error("share overlay service is unavailable: {0}")]
    Unavailable(String),
    /// Host validation or application failed.
    #[error("share overlay service failed: {0}")]
    Internal(String),
}

async fn connection(
    workspaces: &WorkspaceService,
    workspace: &tascarrel_api::types::workspaces::WorkspaceName,
    share: &str,
) -> Result<WorkspaceOverlayShare, Report<ShareOverlayServiceError>> {
    if !tascarrel_protocol::valid_workspace_share_name(share) {
        return Err(invalid("invalid overlay share name"));
    }
    let workspace = tascarrel_protocol::WorkspaceName::new(workspace.as_str())
        .map_err(|_| invalid("invalid workspace name"))?;
    workspaces
        .overlay_share(workspace, share.to_owned())
        .await
        .map_err(|error| unavailable(error.message))
}

async fn prepare(
    channel: Channel,
    request: ShareOverlayRequest,
) -> Result<ShareOverlaySnapshot, Report<ShareOverlayServiceError>> {
    let mut framed = Framed::with_max_frame_len(channel, MAX_SHARE_OVERLAY_FRAME_LEN)
        .map_err(|error| unavailable(error.to_string()))?;
    framed
        .write(&request)
        .await
        .map_err(|error| unavailable(error.to_string()))?;
    match framed
        .read::<ShareOverlayPrepareResponse>()
        .await
        .map_err(|error| unavailable(error.to_string()))?
        .ok_or_else(|| unavailable("guest closed the ShareFS inspection request"))?
    {
        ShareOverlayPrepareResponse::Snapshot { snapshot } => Ok(snapshot),
        ShareOverlayPrepareResponse::RevisionChanged { .. } => Err(internal(
            "guest changed an inspection revision unexpectedly",
        )),
        ShareOverlayPrepareResponse::Error { error } => Err(unavailable(format!(
            "guest could not inspect the ShareFS revision: {}",
            error.message
        ))),
    }
}

async fn send_decision(
    framed: &mut Framed<Channel>,
    decision: ShareOverlayDecision,
) -> Result<(), Report<ShareOverlayServiceError>> {
    framed
        .write(&decision)
        .await
        .map_err(|error| unavailable(error.to_string()))?;
    match framed
        .read::<ShareOverlayCompletion>()
        .await
        .map_err(|error| unavailable(error.to_string()))?
        .ok_or_else(|| unavailable("guest closed before acknowledging the ShareFS decision"))?
    {
        ShareOverlayCompletion::Complete => Ok(()),
        ShareOverlayCompletion::Error { error } => Err(internal(format!(
            "guest could not commit the ShareFS decision: {}",
            error.message
        ))),
    }
}

#[derive(Clone)]
struct ValidatedSnapshot {
    revision: String,
    changes: Vec<ValidatedChange>,
}

#[derive(Clone)]
struct ValidatedChange {
    path: PathBuf,
    components: Vec<OsString>,
    base: Option<ShareOverlayBase>,
    proposed: Option<ValidatedEntry>,
    opaque: bool,
}

#[derive(Clone)]
struct ValidatedEntry {
    version: ShareOverlayEntryVersion,
    contents: Option<Vec<u8>>,
}

fn validate_snapshot(
    snapshot: ShareOverlaySnapshot,
) -> Result<ValidatedSnapshot, Report<ShareOverlayServiceError>> {
    if snapshot.revision.len() != 64
        || !snapshot
            .revision
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
        || snapshot.changes.len() > MAX_SHARE_OVERLAY_CHANGES
    {
        return Err(invalid("guest returned an invalid ShareFS revision"));
    }
    let canonical = serde_json::to_vec(&snapshot.changes)
        .map_err(|error| internal(format!("could not encode ShareFS revision: {error}")))?;
    let revision = hex_digest(&canonical);
    if revision != snapshot.revision {
        return Err(invalid("guest returned a mismatched ShareFS revision"));
    }
    let mut total_content = 0_u64;
    let mut previous = None;
    let mut changes = Vec::with_capacity(snapshot.changes.len());
    for change in snapshot.changes {
        let validated = validate_change(change, &mut total_content)?;
        if previous.as_ref().is_some_and(|path: &PathBuf| {
            path.as_os_str().as_bytes() >= validated.path.as_os_str().as_bytes()
        }) {
            return Err(invalid("ShareFS changes are not strictly path-sorted"));
        }
        previous = Some(validated.path.clone());
        changes.push(validated);
    }
    Ok(ValidatedSnapshot { revision, changes })
}

fn validate_change(
    change: ShareOverlayChange,
    total_content: &mut u64,
) -> Result<ValidatedChange, Report<ShareOverlayServiceError>> {
    if change.path.is_empty() {
        return Err(invalid("ShareFS change contains an empty path"));
    }
    let mut path_bytes = 0_usize;
    let components = change
        .path
        .into_iter()
        .map(|component| {
            let bytes = BASE64
                .decode(component)
                .map_err(|_| invalid("ShareFS path component is not valid Base64"))?;
            path_bytes = path_bytes
                .checked_add(bytes.len() + 1)
                .ok_or_else(|| invalid("ShareFS path length overflowed"))?;
            if bytes.is_empty()
                || bytes == b"."
                || bytes == b".."
                || bytes.contains(&b'/')
                || bytes.contains(&0)
            {
                return Err(invalid("ShareFS path contains an unsafe component"));
            }
            Ok(OsString::from_vec(bytes))
        })
        .collect::<Result<Vec<_>, _>>()?;
    if path_bytes > MAX_PATH_BYTES {
        return Err(invalid("ShareFS path exceeds the supported length"));
    }
    if let Some(base) = &change.base {
        validate_version(&base.version, false)?;
        if base.modified_nanoseconds >= 1_000_000_000 || base.changed_nanoseconds >= 1_000_000_000 {
            return Err(invalid("ShareFS base contains an invalid timestamp"));
        }
    }
    let proposed = change
        .proposed
        .map(|entry| {
            validate_version(&entry.version, true)?;
            let contents = entry
                .contents
                .map(|contents| {
                    BASE64
                        .decode(contents)
                        .map_err(|_| invalid("ShareFS content is not valid Base64"))
                })
                .transpose()?;
            validate_contents(&entry.version, contents.as_deref())?;
            if let Some(contents) = &contents {
                *total_content = total_content
                    .checked_add(contents.len() as u64)
                    .ok_or_else(|| invalid("ShareFS content length overflowed"))?;
                if *total_content > MAX_SHARE_OVERLAY_CONTENT_BYTES {
                    return Err(invalid("ShareFS content exceeds the approval limit"));
                }
            }
            Ok(ValidatedEntry {
                version: entry.version,
                contents,
            })
        })
        .transpose()?;
    if change.base.is_none() && proposed.is_none() {
        return Err(invalid("ShareFS change has neither a base nor a proposal"));
    }
    let path = components.iter().collect::<PathBuf>();
    Ok(ValidatedChange {
        path,
        components,
        base: change.base,
        proposed,
        opaque: change.opaque,
    })
}

fn validate_version(
    version: &ShareOverlayEntryVersion,
    proposed: bool,
) -> Result<(), Report<ShareOverlayServiceError>> {
    if version.mode & !0o7777 != 0 {
        return Err(invalid("ShareFS entry contains invalid permission bits"));
    }
    match version.kind {
        ShareOverlayEntryKind::File | ShareOverlayEntryKind::Symlink => {
            let Some(digest) = &version.content_digest else {
                return Err(invalid("ShareFS content entry has no digest"));
            };
            if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return Err(invalid("ShareFS entry contains an invalid digest"));
            }
        }
        ShareOverlayEntryKind::Directory if version.content_digest.is_some() => {
            return Err(invalid("ShareFS directory unexpectedly has a digest"));
        }
        ShareOverlayEntryKind::Directory => {}
    }
    if proposed && version.size > MAX_SHARE_OVERLAY_CONTENT_BYTES {
        return Err(invalid("ShareFS proposed entry exceeds the approval limit"));
    }
    Ok(())
}

fn validate_contents(
    version: &ShareOverlayEntryVersion,
    contents: Option<&[u8]>,
) -> Result<(), Report<ShareOverlayServiceError>> {
    match (version.kind, contents) {
        (ShareOverlayEntryKind::Directory, None) => return Ok(()),
        (ShareOverlayEntryKind::File | ShareOverlayEntryKind::Symlink, Some(contents)) => {
            let digest = hex_digest(contents);
            if contents.len() as u64 != version.size
                || version.content_digest.as_deref() != Some(digest.as_str())
            {
                return Err(invalid(
                    "ShareFS proposed contents do not match their version",
                ));
            }
            if version.kind == ShareOverlayEntryKind::Symlink && contents.contains(&0) {
                return Err(invalid("ShareFS symbolic-link target contains NUL"));
            }
            return Ok(());
        }
        _ => {}
    }
    Err(invalid(
        "ShareFS proposed entry has incompatible content representation",
    ))
}

fn summaries(snapshot: &ValidatedSnapshot) -> tascarrel_api::ArcVec<api::ShareOverlayChange> {
    snapshot
        .changes
        .iter()
        .map(|change| api::ShareOverlayChange {
            path: change.path.to_string_lossy().into_owned().into(),
            base_kind: change.base.as_ref().map(|base| api_kind(base.version.kind)),
            proposed_kind: change
                .proposed
                .as_ref()
                .map(|entry| api_kind(entry.version.kind)),
            proposed_size: change.proposed.as_ref().and_then(|entry| {
                (entry.version.kind == ShareOverlayEntryKind::File).then_some(entry.version.size)
            }),
        })
        .collect::<Vec<_>>()
        .into()
}

fn api_kind(kind: ShareOverlayEntryKind) -> api::ShareOverlayEntryKind {
    match kind {
        ShareOverlayEntryKind::File => api::ShareOverlayEntryKind::File,
        ShareOverlayEntryKind::Directory => api::ShareOverlayEntryKind::Directory,
        ShareOverlayEntryKind::Symlink => api::ShareOverlayEntryKind::Symlink,
    }
}

enum ApplyAttempt {
    Applied,
    Conflicts(Vec<api::ShareOverlayConflict>),
}

fn validate_and_apply(
    root: &File,
    snapshot: &ValidatedSnapshot,
) -> Result<ApplyAttempt, Report<ShareOverlayServiceError>> {
    let conflicts = conflicts(root, snapshot)?;
    if !conflicts.is_empty() {
        return Ok(ApplyAttempt::Conflicts(conflicts));
    }
    apply_changes(root, &snapshot.changes)?;
    Ok(ApplyAttempt::Applied)
}

fn conflicts(
    root: &File,
    snapshot: &ValidatedSnapshot,
) -> Result<Vec<api::ShareOverlayConflict>, Report<ShareOverlayServiceError>> {
    let root = duplicate_root(root)?;
    let proposed_directories = snapshot
        .changes
        .iter()
        .filter_map(|change| {
            change
                .proposed
                .as_ref()
                .is_some_and(|entry| entry.version.kind == ShareOverlayEntryKind::Directory)
                .then_some(change.path.clone())
        })
        .collect::<HashSet<_>>();
    let mut conflicts = Vec::new();
    for change in &snapshot.changes {
        let current = match current_entry(&root, &change.components)? {
            CurrentEntryLookup::Entry(current) => current,
            CurrentEntryLookup::BlockedParent(parent) if proposed_directories.contains(&parent) => {
                None
            }
            CurrentEntryLookup::BlockedParent(parent) => {
                conflicts.push(api::ShareOverlayConflict {
                    path: change.path.to_string_lossy().into_owned().into(),
                    reason: format!(
                        "the current host parent {} is absent or is not a directory",
                        parent.display()
                    )
                    .into(),
                    text_diff: None,
                });
                continue;
            }
            CurrentEntryLookup::ConcurrentChange => {
                conflicts.push(api::ShareOverlayConflict {
                    path: change.path.to_string_lossy().into_owned().into(),
                    reason: "the current host entry changed while it was being inspected".into(),
                    text_diff: None,
                });
                continue;
            }
            CurrentEntryLookup::Unsupported => {
                conflicts.push(api::ShareOverlayConflict {
                    path: change.path.to_string_lossy().into_owned().into(),
                    reason: "the current host entry has an unsupported type".into(),
                    text_diff: None,
                });
                continue;
            }
        };
        let matches = match (&change.base, &current) {
            (None, None) => true,
            (Some(base), Some(current)) => matches_base(base, current),
            _ => false,
        };
        if matches {
            continue;
        }
        let text_diff = current
            .as_ref()
            .and_then(|current| current.contents.as_deref())
            .zip(
                change
                    .proposed
                    .as_ref()
                    .and_then(|entry| entry.contents.as_deref()),
            )
            .and_then(|(current, proposed)| text_diff(current, proposed));
        conflicts.push(api::ShareOverlayConflict {
            path: change.path.to_string_lossy().into_owned().into(),
            reason: "the current host entry no longer matches the captured base".into(),
            text_diff: text_diff.map(Into::into),
        });
    }
    Ok(conflicts)
}

enum CurrentEntryLookup {
    Entry(Option<CurrentEntry>),
    BlockedParent(PathBuf),
    ConcurrentChange,
    Unsupported,
}

struct CurrentEntry {
    version: ShareOverlayEntryVersion,
    modified_seconds: i64,
    modified_nanoseconds: u32,
    changed_seconds: i64,
    changed_nanoseconds: u32,
    contents: Option<Vec<u8>>,
}

enum CurrentParent<'a> {
    Open { directory: OwnedFd, name: &'a OsStr },
    Blocked(PathBuf),
}

enum CurrentContents {
    Stable {
        digest: Option<String>,
        contents: Option<Vec<u8>>,
    },
    ConcurrentChange,
}

fn matches_base(base: &ShareOverlayBase, current: &CurrentEntry) -> bool {
    let cheap_version_match = base.version.kind == current.version.kind
        && base.version.mode == current.version.mode
        && (base.version.kind == ShareOverlayEntryKind::Directory
            || base.version.size == current.version.size);
    if cheap_version_match
        && base.modified_seconds == current.modified_seconds
        && base.modified_nanoseconds == current.modified_nanoseconds
        && base.changed_seconds == current.changed_seconds
        && base.changed_nanoseconds == current.changed_nanoseconds
    {
        return true;
    }
    base.version.kind != ShareOverlayEntryKind::Directory && base.version == current.version
}

fn current_entry(
    root: &OwnedFd,
    components: &[OsString],
) -> Result<CurrentEntryLookup, Report<ShareOverlayServiceError>> {
    let (directory, name) = match current_parent(root, components)? {
        CurrentParent::Open { directory, name } => (directory, name),
        CurrentParent::Blocked(parent) => {
            return Ok(CurrentEntryLookup::BlockedParent(parent));
        }
    };
    let stat = match rustix::fs::statat(&directory, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => stat,
        Err(rustix::io::Errno::NOENT) => return Ok(CurrentEntryLookup::Entry(None)),
        Err(error) => return Err(io_error("inspect current host share entry", error)),
    };
    let Ok(kind) = entry_kind(&stat) else {
        return Ok(CurrentEntryLookup::Unsupported);
    };
    let CurrentContents::Stable { digest, contents } =
        current_contents(&directory, name, &stat, kind)?
    else {
        return Ok(CurrentEntryLookup::ConcurrentChange);
    };
    let modified_nanoseconds = u32::try_from(stat.st_mtime_nsec)
        .map_err(|_| invalid("host share entry has an invalid modification timestamp"))?;
    let changed_nanoseconds = u32::try_from(stat.st_ctime_nsec)
        .map_err(|_| invalid("host share entry has an invalid change timestamp"))?;
    Ok(CurrentEntryLookup::Entry(Some(CurrentEntry {
        version: ShareOverlayEntryVersion {
            kind,
            size: u64::try_from(stat.st_size)
                .map_err(|_| invalid("host share entry has a negative size"))?,
            mode: u32::try_from(u64::from(stat.st_mode & 0o7777))
                .map_err(|_| internal("host share mode does not fit the protocol mode type"))?,
            content_digest: digest,
        },
        modified_seconds: stat.st_mtime,
        modified_nanoseconds,
        changed_seconds: stat.st_ctime,
        changed_nanoseconds,
        contents,
    })))
}

fn current_parent<'a>(
    root: &OwnedFd,
    components: &'a [OsString],
) -> Result<CurrentParent<'a>, Report<ShareOverlayServiceError>> {
    let (name, parents) = components
        .split_last()
        .ok_or_else(|| invalid("ShareFS path has no final component"))?;
    let mut parent = rustix::fs::openat(
        root,
        ".",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|error| io_error("duplicate pinned overlay host share", error))?;
    let mut parent_path = PathBuf::new();
    for component in parents {
        parent_path.push(component);
        parent = match rustix::fs::openat(
            &parent,
            component,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(directory) => directory,
            Err(rustix::io::Errno::NOENT | rustix::io::Errno::NOTDIR | rustix::io::Errno::LOOP) => {
                return Ok(CurrentParent::Blocked(parent_path));
            }
            Err(error) => return Err(io_error("traverse pinned overlay host share", error)),
        };
    }
    Ok(CurrentParent::Open {
        directory: parent,
        name,
    })
}

fn current_contents(
    parent: &OwnedFd,
    name: &OsStr,
    stat: &Stat,
    kind: ShareOverlayEntryKind,
) -> Result<CurrentContents, Report<ShareOverlayServiceError>> {
    match kind {
        ShareOverlayEntryKind::File => current_file_contents(parent, name, stat),
        ShareOverlayEntryKind::Symlink => current_symlink_contents(parent, name, stat),
        ShareOverlayEntryKind::Directory => {
            if current_path_matches(parent, name, stat, "reinspect host share directory")? {
                Ok(CurrentContents::Stable {
                    digest: None,
                    contents: None,
                })
            } else {
                Ok(CurrentContents::ConcurrentChange)
            }
        }
    }
}

fn current_file_contents(
    parent: &OwnedFd,
    name: &OsStr,
    stat: &Stat,
) -> Result<CurrentContents, Report<ShareOverlayServiceError>> {
    let fd = rustix::fs::openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(|error| io_error("open current host share file", error))?;
    let mut file = File::from(fd);
    let opened_before = rustix::fs::fstat(file.as_fd())
        .map_err(|error| io_error("inspect opened host share file", error))?;
    if !same_stat_fingerprint(stat, &opened_before) {
        return Ok(CurrentContents::ConcurrentChange);
    }
    let mut hasher = Sha256::new();
    let mut retained = Vec::new();
    let mut buffer = vec![0_u8; 128 * 1024].into_boxed_slice();
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|error| internal(format!("read current host share file: {error}")))?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
        if retained.len() <= MAX_TEXT_DIFF_BYTES {
            retained.extend_from_slice(&buffer[..count]);
        }
    }
    let opened_after = rustix::fs::fstat(file.as_fd())
        .map_err(|error| io_error("reinspect opened host share file", error))?;
    if !same_stat_fingerprint(stat, &opened_after)
        || !current_path_matches(parent, name, stat, "reinspect host share file path")?
    {
        return Ok(CurrentContents::ConcurrentChange);
    }
    Ok(CurrentContents::Stable {
        digest: Some(hex_bytes(hasher.finalize().as_slice())),
        contents: (retained.len() <= MAX_TEXT_DIFF_BYTES).then_some(retained),
    })
}

fn current_symlink_contents(
    parent: &OwnedFd,
    name: &OsStr,
    stat: &Stat,
) -> Result<CurrentContents, Report<ShareOverlayServiceError>> {
    let target = rustix::fs::readlinkat(parent, name, Vec::new())
        .map_err(|error| io_error("read current host share symbolic link", error))?
        .to_bytes()
        .to_vec();
    if !current_path_matches(parent, name, stat, "reinspect host share symbolic link")? {
        return Ok(CurrentContents::ConcurrentChange);
    }
    Ok(CurrentContents::Stable {
        digest: Some(hex_digest(&target)),
        contents: (target.len() <= MAX_TEXT_DIFF_BYTES).then_some(target),
    })
}

fn current_path_matches(
    parent: &OwnedFd,
    name: &OsStr,
    expected: &Stat,
    operation: &'static str,
) -> Result<bool, Report<ShareOverlayServiceError>> {
    match rustix::fs::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(actual) => Ok(same_stat_fingerprint(expected, &actual)),
        Err(rustix::io::Errno::NOENT) => Ok(false),
        Err(error) => Err(io_error(operation, error)),
    }
}

fn same_stat_fingerprint(left: &Stat, right: &Stat) -> bool {
    left.st_dev == right.st_dev
        && left.st_ino == right.st_ino
        && left.st_mode == right.st_mode
        && left.st_size == right.st_size
        && left.st_mtime == right.st_mtime
        && left.st_mtime_nsec == right.st_mtime_nsec
        && left.st_ctime == right.st_ctime
        && left.st_ctime_nsec == right.st_ctime_nsec
}

fn text_diff(current: &[u8], proposed: &[u8]) -> Option<String> {
    let current = std::str::from_utf8(current).ok()?;
    let proposed = std::str::from_utf8(proposed).ok()?;
    Some(
        TextDiff::from_lines(current, proposed)
            .unified_diff()
            .header("current", "proposed")
            .to_string(),
    )
}

fn apply_changes(
    root: &File,
    changes: &[ValidatedChange],
) -> Result<(), Report<ShareOverlayServiceError>> {
    let root = duplicate_root(root)?;
    let mut removals = changes
        .iter()
        .filter(|change| {
            change.proposed.is_none()
                || change.opaque
                || change
                    .base
                    .as_ref()
                    .zip(change.proposed.as_ref())
                    .is_some_and(|(base, proposed)| base.version.kind != proposed.version.kind)
        })
        .collect::<Vec<_>>();
    removals.sort_by_key(|change| std::cmp::Reverse(change.components.len()));
    for change in removals {
        remove_existing(
            &root,
            &change.components,
            change.opaque
                || change
                    .base
                    .as_ref()
                    .is_some_and(|base| base.version.kind == ShareOverlayEntryKind::Directory),
        )?;
    }

    let mut directories = changes
        .iter()
        .filter(|change| {
            change
                .proposed
                .as_ref()
                .is_some_and(|entry| entry.version.kind == ShareOverlayEntryKind::Directory)
        })
        .collect::<Vec<_>>();
    directories.sort_by_key(|change| change.components.len());
    for change in directories {
        write_directory(&root, change)?;
    }
    for change in changes {
        let Some(proposed) = &change.proposed else {
            continue;
        };
        match proposed.version.kind {
            ShareOverlayEntryKind::File => write_file(&root, change, proposed)?,
            ShareOverlayEntryKind::Symlink => write_symlink(&root, change, proposed)?,
            ShareOverlayEntryKind::Directory => {}
        }
    }
    Ok(())
}

fn write_directory(
    root: &OwnedFd,
    change: &ValidatedChange,
) -> Result<(), Report<ShareOverlayServiceError>> {
    let proposed = change
        .proposed
        .as_ref()
        .ok_or_else(|| internal("missing proposed ShareFS directory"))?;
    let (parent, name) = open_parent(root, &change.components)?;
    let mode = host_mode(proposed.version.mode)?;
    match rustix::fs::mkdirat(&parent, name, mode) {
        Ok(()) | Err(rustix::io::Errno::EXIST) => {}
        Err(error) => return Err(io_error("create approved host share directory", error)),
    }
    let directory = rustix::fs::openat(
        &parent,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|error| io_error("open approved host share directory", error))?;
    rustix::fs::fchmod(&directory, mode)
        .map_err(|error| io_error("set approved host share directory mode", error))
}

fn write_file(
    root: &OwnedFd,
    change: &ValidatedChange,
    proposed: &ValidatedEntry,
) -> Result<(), Report<ShareOverlayServiceError>> {
    let contents = proposed
        .contents
        .as_deref()
        .ok_or_else(|| internal("missing approved file contents"))?;
    let (parent, name) = open_parent(root, &change.components)?;
    let mode = host_mode(proposed.version.mode)?;
    let temporary = OsString::from(format!(".tascarrel-sharefs-{}", uuid::Uuid::new_v4()));
    let fd = rustix::fs::openat(
        &parent,
        &temporary,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o600),
    )
    .map_err(|error| io_error("create approved host share temporary file", error))?;
    let mut file = File::from(fd);
    let result = (|| {
        file.write_all(contents)
            .map_err(|error| internal(format!("write approved host share file: {error}")))?;
        rustix::fs::fchmod(file.as_fd(), mode)
            .map_err(|error| io_error("set approved host share file mode", error))?;
        file.sync_all()
            .map_err(|error| internal(format!("sync approved host share file: {error}")))?;
        rustix::fs::renameat(&parent, &temporary, &parent, name)
            .map_err(|error| io_error("publish approved host share file", error))
    })();
    if result.is_err()
        && let Err(error) = rustix::fs::unlinkat(&parent, &temporary, AtFlags::empty())
    {
        tracing::warn!(
            temporary = ?temporary,
            %error,
            "could not remove an unpublished host-share file"
        );
    }
    result
}

fn write_symlink(
    root: &OwnedFd,
    change: &ValidatedChange,
    proposed: &ValidatedEntry,
) -> Result<(), Report<ShareOverlayServiceError>> {
    let target = proposed
        .contents
        .as_deref()
        .ok_or_else(|| internal("missing approved symbolic-link target"))?;
    let target = OsStr::from_bytes(target);
    let (parent, name) = open_parent(root, &change.components)?;
    let temporary = OsString::from(format!(".tascarrel-sharefs-{}", uuid::Uuid::new_v4()));
    rustix::fs::symlinkat(target, &parent, &temporary)
        .map_err(|error| io_error("create approved host share symbolic link", error))?;
    let result = rustix::fs::renameat(&parent, &temporary, &parent, name)
        .map_err(|error| io_error("publish approved host share symbolic link", error));
    if result.is_err()
        && let Err(error) = rustix::fs::unlinkat(&parent, &temporary, AtFlags::empty())
    {
        tracing::warn!(
            temporary = ?temporary,
            %error,
            "could not remove an unpublished host-share symbolic link"
        );
    }
    result
}

fn remove_existing(
    root: &OwnedFd,
    components: &[OsString],
    recursive: bool,
) -> Result<(), Report<ShareOverlayServiceError>> {
    let (parent, name) = open_parent(root, components)?;
    let stat = match rustix::fs::statat(&parent, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => stat,
        Err(rustix::io::Errno::NOENT) => return Ok(()),
        Err(error) => return Err(io_error("inspect approved removal target", error)),
    };
    if entry_kind(&stat)? == ShareOverlayEntryKind::Directory {
        if recursive {
            let directory = rustix::fs::openat(
                &parent,
                name,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(|error| io_error("open approved recursive removal", error))?;
            remove_directory_contents(&directory)?;
        }
        rustix::fs::unlinkat(&parent, name, AtFlags::REMOVEDIR)
            .map_err(|error| io_error("remove approved host share directory", error))
    } else {
        rustix::fs::unlinkat(&parent, name, AtFlags::empty())
            .map_err(|error| io_error("remove approved host share entry", error))
    }
}

fn remove_directory_contents(directory: &OwnedFd) -> Result<(), Report<ShareOverlayServiceError>> {
    let entries = rustix::fs::Dir::read_from(directory)
        .map_err(|error| io_error("enumerate approved recursive removal", error))?
        .map(|entry| {
            entry
                .map(|entry| OsString::from_vec(entry.file_name().to_bytes().to_vec()))
                .map_err(|error| io_error("read approved recursive removal entry", error))
        })
        .collect::<Result<Vec<_>, _>>()?;
    for name in entries {
        if name == "." || name == ".." {
            continue;
        }
        let stat = rustix::fs::statat(directory, &name, AtFlags::SYMLINK_NOFOLLOW)
            .map_err(|error| io_error("inspect approved recursive removal entry", error))?;
        if entry_kind(&stat)? == ShareOverlayEntryKind::Directory {
            let child = rustix::fs::openat(
                directory,
                &name,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(|error| io_error("open approved recursive removal directory", error))?;
            remove_directory_contents(&child)?;
            rustix::fs::unlinkat(directory, &name, AtFlags::REMOVEDIR)
                .map_err(|error| io_error("remove approved recursive directory", error))?;
        } else {
            rustix::fs::unlinkat(directory, &name, AtFlags::empty())
                .map_err(|error| io_error("remove approved recursive entry", error))?;
        }
    }
    Ok(())
}

fn duplicate_root(root: &File) -> Result<OwnedFd, Report<ShareOverlayServiceError>> {
    rustix::fs::openat(
        root,
        ".",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|error| io_error("duplicate pinned overlay host share", error))
}

fn open_parent<'a>(
    root: &OwnedFd,
    components: &'a [OsString],
) -> Result<(OwnedFd, &'a OsStr), Report<ShareOverlayServiceError>> {
    let (name, parents) = components
        .split_last()
        .ok_or_else(|| invalid("ShareFS path has no final component"))?;
    let mut directory = rustix::fs::openat(
        root,
        ".",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|error| io_error("duplicate pinned overlay host share", error))?;
    for component in parents {
        directory = rustix::fs::openat(
            &directory,
            component,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|error| io_error("traverse pinned overlay host share", error))?;
    }
    Ok((directory, name))
}

fn entry_kind(stat: &Stat) -> Result<ShareOverlayEntryKind, Report<ShareOverlayServiceError>> {
    let kind = FileType::from_raw_mode(stat.st_mode);
    if kind.is_file() {
        Ok(ShareOverlayEntryKind::File)
    } else if kind.is_dir() {
        Ok(ShareOverlayEntryKind::Directory)
    } else if kind.is_symlink() {
        Ok(ShareOverlayEntryKind::Symlink)
    } else {
        Err(invalid("host share contains an unsupported entry type"))
    }
}

/// Converts a validated protocol mode to the host platform's raw mode type.
fn host_mode(mode: u32) -> Result<Mode, Report<ShareOverlayServiceError>> {
    let mode = rustix::fs::RawMode::try_from(mode)
        .map_err(|_| internal("validated ShareFS mode does not fit the host mode type"))?;
    Ok(Mode::from_raw_mode(mode))
}

fn hex_digest(bytes: &[u8]) -> String {
    hex_bytes(Sha256::digest(bytes).as_slice())
}

fn hex_bytes(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(encoded, "{byte:02x}").expect("writing to a String cannot fail");
    }
    encoded
}

fn invalid(message: impl Into<String>) -> Report<ShareOverlayServiceError> {
    ShareOverlayServiceError::InvalidRequest(message.into()).report()
}

fn unavailable(message: impl Into<String>) -> Report<ShareOverlayServiceError> {
    ShareOverlayServiceError::Unavailable(message.into()).report()
}

fn internal(message: impl Into<String>) -> Report<ShareOverlayServiceError> {
    ShareOverlayServiceError::Internal(message.into()).report()
}

fn io_error(operation: &'static str, error: rustix::io::Errno) -> Report<ShareOverlayServiceError> {
    internal(format!("{operation}: {error}"))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::path::Path;

    use tempfile::tempdir;

    use super::*;

    fn open_root(path: &Path) -> Result<File, Report<ShareOverlayServiceError>> {
        rustix::fs::open(
            path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map(File::from)
        .map_err(|error| io_error("open pinned overlay host share", error))
    }

    fn components(path: &str) -> Vec<OsString> {
        Path::new(path)
            .components()
            .map(|component| component.as_os_str().to_owned())
            .collect()
    }

    fn file_entry(contents: &[u8]) -> ValidatedEntry {
        ValidatedEntry {
            version: ShareOverlayEntryVersion {
                kind: ShareOverlayEntryKind::File,
                size: contents.len() as u64,
                mode: 0o640,
                content_digest: Some(hex_digest(contents)),
            },
            contents: Some(contents.to_vec()),
        }
    }

    fn directory_entry() -> ValidatedEntry {
        ValidatedEntry {
            version: ShareOverlayEntryVersion {
                kind: ShareOverlayEntryKind::Directory,
                size: 0,
                mode: 0o750,
                content_digest: None,
            },
            contents: None,
        }
    }

    fn change(path: &str, proposed: Option<ValidatedEntry>) -> ValidatedChange {
        ValidatedChange {
            path: path.into(),
            components: components(path),
            base: None,
            proposed,
            opaque: false,
        }
    }

    fn present_current(root: &OwnedFd, path: &str) -> CurrentEntry {
        match current_entry(root, &components(path)).unwrap() {
            CurrentEntryLookup::Entry(Some(current)) => current,
            _ => panic!("expected a current host entry"),
        }
    }

    /// Verifies approved additions, replacements, removals, and opaque
    /// directories are materialized as one deterministic host operation.
    #[test]
    fn applies_validated_change_kinds() {
        let temporary = tempdir().unwrap();
        let root = temporary.path();
        fs::write(root.join("document"), b"base").unwrap();
        fs::write(root.join("victim"), b"remove").unwrap();
        fs::create_dir(root.join("opaque")).unwrap();
        fs::write(root.join("opaque/hidden"), b"hidden").unwrap();

        let mut document = change("document", Some(file_entry(b"proposal")));
        document.base = Some(ShareOverlayBase {
            version: file_entry(b"base").version,
            modified_seconds: 0,
            modified_nanoseconds: 0,
            changed_seconds: 0,
            changed_nanoseconds: 0,
        });
        let mut victim = change("victim", None);
        victim.base = Some(ShareOverlayBase {
            version: file_entry(b"remove").version,
            modified_seconds: 0,
            modified_nanoseconds: 0,
            changed_seconds: 0,
            changed_nanoseconds: 0,
        });
        let mut opaque = change("opaque", Some(directory_entry()));
        opaque.base = Some(ShareOverlayBase {
            version: ShareOverlayEntryVersion {
                kind: ShareOverlayEntryKind::Directory,
                size: 0,
                mode: 0o755,
                content_digest: None,
            },
            modified_seconds: 0,
            modified_nanoseconds: 0,
            changed_seconds: 0,
            changed_nanoseconds: 0,
        });
        opaque.opaque = true;
        let directory = change("added", Some(directory_entry()));
        let nested = change("added/nested", Some(file_entry(b"new")));
        let pinned = open_root(root).unwrap();

        apply_changes(&pinned, &[directory, nested, document, victim, opaque]).unwrap();

        assert_eq!(fs::read(root.join("document")).unwrap(), b"proposal");
        assert!(!root.join("victim").exists());
        assert!(fs::read_dir(root.join("opaque")).unwrap().next().is_none());
        assert_eq!(fs::read(root.join("added/nested")).unwrap(), b"new");
    }

    /// Verifies the complete approval path accepts children of a directory
    /// which is created by the same exact revision.
    #[test]
    fn validates_and_applies_nested_additions() {
        let temporary = tempdir().unwrap();
        let root = temporary.path();
        let snapshot = ValidatedSnapshot {
            revision: "0".repeat(64),
            changes: vec![
                change("added", Some(directory_entry())),
                change("added/nested", Some(file_entry(b"new"))),
            ],
        };
        let pinned = open_root(root).unwrap();

        let attempted = validate_and_apply(&pinned, &snapshot).unwrap();

        assert!(matches!(attempted, ApplyAttempt::Applied));
        assert_eq!(fs::read(root.join("added/nested")).unwrap(), b"new");
    }

    /// Verifies a concurrent host text edit becomes a conflict with a
    /// current-to-proposed unified diff.
    #[test]
    fn reports_concurrent_text_change_with_diff() {
        let temporary = tempdir().unwrap();
        let root = temporary.path();
        fs::write(root.join("document"), b"base\n").unwrap();
        let descriptor = open_root(root).unwrap();
        let current = present_current(&duplicate_root(&descriptor).unwrap(), "document");
        let base = ShareOverlayBase {
            version: current.version,
            modified_seconds: current.modified_seconds,
            modified_nanoseconds: current.modified_nanoseconds,
            changed_seconds: current.changed_seconds,
            changed_nanoseconds: current.changed_nanoseconds,
        };
        fs::write(root.join("document"), b"host\n").unwrap();
        let mut proposed = change("document", Some(file_entry(b"pod\n")));
        proposed.base = Some(base);
        let snapshot = ValidatedSnapshot {
            revision: "0".repeat(64),
            changes: vec![proposed],
        };

        let conflicts = conflicts(&descriptor, &snapshot).unwrap();

        assert_eq!(conflicts.len(), 1);
        let diff = conflicts[0].text_diff.as_ref().unwrap();
        assert!(diff.contains("-host"));
        assert!(diff.contains("+pod"));
    }

    /// Verifies untrusted paths cannot traverse a symbolic-link parent outside
    /// the host-pinned share directory.
    #[test]
    fn rejects_symbolic_link_parent_traversal() {
        let temporary = tempdir().unwrap();
        let root = temporary.path().join("root");
        let outside = temporary.path().join("outside");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("victim"), b"outside").unwrap();
        symlink(&outside, root.join("escape")).unwrap();

        let pinned = open_root(&root).unwrap();
        let error = apply_changes(
            &pinned,
            &[change("escape/victim", Some(file_entry(b"proposal")))],
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("traverse pinned overlay host share")
        );
        assert_eq!(fs::read(outside.join("victim")).unwrap(), b"outside");
    }
}
