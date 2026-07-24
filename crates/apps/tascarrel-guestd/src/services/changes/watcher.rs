//! Fanotify invalidation for one pod workspace mount.
//!
//! [`WorkspaceWatcher`] combines mount-wide content events with directory
//! structure marks. It reduces kernel events to conservative workspace paths;
//! an unresolved or overflow event requests a full repository rescan.

use std::io;
use std::os::fd::AsRawFd as _;
use std::os::fd::OwnedFd;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use nix::errno::Errno;
use nix::fcntl::AT_FDCWD;
use nix::libc;
use nix::sys::fanotify::EventFFlags;
use nix::sys::fanotify::Fanotify;
use nix::sys::fanotify::InitFlags;
use nix::sys::fanotify::MarkFlags;
use nix::sys::fanotify::MaskFlags;
use reportify::ErrorExt as _;
use reportify::Report;
use thiserror::Error;
use tokio::io::unix::AsyncFd;
use tracing::debug;

use crate::services::files::open_directory;

/// Content and structure event sources for one pod workspace.
pub(crate) struct WorkspaceWatcher {
    content: AsyncFd<Fanotify>,
    structure: AsyncFd<Fanotify>,
    root: PathBuf,
    watch_mount: Arc<OwnedFd>,
    event_debounce: std::time::Duration,
    max_event_batch: usize,
}

impl WorkspaceWatcher {
    /// Opens and marks fanotify descriptors for one pod workspace.
    ///
    /// # Errors
    ///
    /// Returns a report when the workspace cannot be resolved or fanotify
    /// initialization fails.
    pub(crate) fn new(
        workspace: &Path,
        watch_mount: Arc<OwnedFd>,
        event_debounce: std::time::Duration,
        max_event_batch: usize,
    ) -> Result<Self, Report<WorkspaceWatcherError>> {
        let root = std::fs::canonicalize(workspace)
            .map_err(|error| watcher_failed("failed to resolve workspace watch root", error))?;
        let root_descriptor = open_directory(workspace, "").map_err(|report| {
            report.escalate(WorkspaceWatcherError::Failed(
                "failed to open workspace watch root".to_owned(),
            ))
        })?;
        let content = Fanotify::init(
            InitFlags::FAN_CLASS_NOTIF | InitFlags::FAN_CLOEXEC | InitFlags::FAN_NONBLOCK,
            EventFFlags::O_RDONLY | EventFFlags::O_LARGEFILE | EventFFlags::O_CLOEXEC,
        )
        .map_err(|error| {
            watcher_failed("failed to initialize workspace content fanotify", error)
        })?;
        let watch_mount_path = descriptor_path(&watch_mount);
        content
            .mark(
                MarkFlags::FAN_MARK_ADD | MarkFlags::FAN_MARK_MOUNT,
                content_events(),
                AT_FDCWD,
                Some(&watch_mount_path),
            )
            .map_err(|error| {
                watcher_failed("failed to watch pod workspace mount with fanotify", error)
            })?;
        let structure = Fanotify::init(
            InitFlags::FAN_CLASS_NOTIF
                | InitFlags::FAN_CLOEXEC
                | InitFlags::FAN_NONBLOCK
                | InitFlags::from_bits_retain(libc::FAN_REPORT_DFID_NAME),
            EventFFlags::O_RDONLY | EventFFlags::O_LARGEFILE | EventFFlags::O_CLOEXEC,
        )
        .map_err(|error| {
            watcher_failed("failed to initialize workspace structure fanotify", error)
        })?;
        let watcher = Self {
            content: AsyncFd::new(content).map_err(|error| {
                watcher_failed("failed to register content fanotify descriptor", error)
            })?,
            structure: AsyncFd::new(structure).map_err(|error| {
                watcher_failed("failed to register structure fanotify descriptor", error)
            })?,
            root,
            watch_mount,
            event_debounce,
            max_event_batch,
        };
        watcher.watch_descriptor(&root_descriptor)?;
        Ok(watcher)
    }

    /// Returns whether this watcher is attached to the current pod mount.
    pub(crate) fn watches_mount(&self, watch_mount: &Arc<OwnedFd>) -> bool {
        Arc::ptr_eq(&self.watch_mount, watch_mount)
    }

    /// Adds a structure-event mark for one directory visited by the files API.
    ///
    /// # Errors
    ///
    /// Returns a report when the directory cannot be opened or marked.
    pub(crate) fn watch_directory(
        &self,
        workspace: &Path,
        relative: &str,
    ) -> Result<(), Report<WorkspaceWatcherError>> {
        let descriptor = open_directory(workspace, relative).map_err(|report| {
            report.escalate(WorkspaceWatcherError::Failed(
                "failed to open workspace directory for watching".to_owned(),
            ))
        })?;
        self.watch_descriptor(&descriptor)
    }

    fn watch_descriptor(&self, directory: &OwnedFd) -> Result<(), Report<WorkspaceWatcherError>> {
        let descriptor_path = descriptor_path(directory);
        self.structure
            .get_ref()
            .mark(
                MarkFlags::FAN_MARK_ADD | MarkFlags::FAN_MARK_ONLYDIR,
                structure_events(),
                AT_FDCWD,
                Some(&descriptor_path),
            )
            .map_err(|error| {
                watcher_failed(
                    format!(
                        "failed to watch workspace directory {} with fanotify",
                        descriptor_path.display()
                    ),
                    error,
                )
            })
    }

    /// Waits for and coalesces one bounded batch of workspace mutations.
    ///
    /// # Errors
    ///
    /// Returns a report when fanotify event delivery fails.
    pub(crate) async fn next_batch(
        &self,
    ) -> Result<Vec<WorkspaceEvent>, Report<WorkspaceWatcherError>> {
        let mut output = self.read_events().await?;
        let deadline = tokio::time::Instant::now() + self.event_debounce;
        while output.len() < self.max_event_batch {
            match tokio::time::timeout_at(deadline, self.read_events()).await {
                Ok(Ok(events)) => output.extend(events),
                Ok(Err(error)) => return Err(error),
                Err(_) => break,
            }
        }
        if output.len() > self.max_event_batch {
            output.truncate(self.max_event_batch - 1);
            output.push(WorkspaceEvent {
                path: None,
                overflow: true,
            });
        }
        Ok(output)
    }

    async fn read_events(&self) -> Result<Vec<WorkspaceEvent>, Report<WorkspaceWatcherError>> {
        loop {
            let mut fanotify = tokio::select! {
                ready = self.content.readable() => ready
                    .map_err(|error| watcher_failed("failed to wait for workspace content event", error))?,
                ready = self.structure.readable() => ready
                    .map_err(|error| watcher_failed("failed to wait for workspace structure event", error))?,
            };
            match fanotify
                .try_io(|descriptor| descriptor.get_ref().read_events().map_err(errno_to_io))
            {
                Ok(Ok(events)) => {
                    return Ok(events
                        .into_iter()
                        .filter_map(|event| {
                            let mask = event.mask();
                            if !mask.intersects(mutation_events()) {
                                return None;
                            }
                            let path = event.fd().and_then(|fd| {
                                let descriptor_path = format!("/proc/self/fd/{}", fd.as_raw_fd());
                                let path = match std::fs::read_link(&descriptor_path) {
                                    Ok(path) => path,
                                    Err(error) => {
                                        debug!(%error, descriptor_path, "failed to resolve fanotify event path");
                                        return None;
                                    }
                                };
                                event_relative_path(&path, &self.root)
                                    .and_then(|path| path.to_str().map(str::to_owned))
                                    .filter(|path| !path.is_empty())
                            });
                            Some(WorkspaceEvent {
                                path,
                                overflow: mask.contains(MaskFlags::FAN_Q_OVERFLOW),
                            })
                        })
                        .collect());
                }
                Ok(Err(error)) => {
                    return Err(watcher_failed(
                        "failed to read workspace fanotify events",
                        error,
                    ));
                }
                Err(_) => {}
            }
        }
    }
}

fn descriptor_path(descriptor: &OwnedFd) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", descriptor.as_raw_fd()))
}

/// Conservative invalidation derived from one fanotify event.
#[derive(Clone, Debug)]
pub(crate) struct WorkspaceEvent {
    /// Workspace-relative path, or absent when all repositories must refresh.
    pub(crate) path: Option<String>,
    /// Whether the kernel or userspace event buffer overflowed.
    pub(crate) overflow: bool,
}

/// Failure from workspace mutation monitoring.
#[derive(Debug, Error)]
pub(crate) enum WorkspaceWatcherError {
    /// Fanotify initialization, marking, or event delivery failed.
    #[error("workspace watcher failed: {0}")]
    Failed(String),
}

fn event_relative_path(path: &Path, root: &Path) -> Option<PathBuf> {
    if let Ok(relative) = path.strip_prefix(root) {
        Some(relative.to_owned())
    } else if let Ok(relative) = path.strip_prefix("/workspace") {
        Some(relative.to_owned())
    } else {
        None
    }
}

fn content_events() -> MaskFlags {
    MaskFlags::FAN_MODIFY | MaskFlags::FAN_CLOSE_WRITE
}

fn structure_events() -> MaskFlags {
    MaskFlags::FAN_ATTRIB
        | MaskFlags::FAN_MOVED_FROM
        | MaskFlags::FAN_MOVED_TO
        | MaskFlags::FAN_CREATE
        | MaskFlags::FAN_DELETE
        | MaskFlags::FAN_DELETE_SELF
        | MaskFlags::FAN_MOVE_SELF
        | MaskFlags::FAN_RENAME
        | MaskFlags::FAN_EVENT_ON_CHILD
        | MaskFlags::FAN_ONDIR
}

fn mutation_events() -> MaskFlags {
    content_events()
        | MaskFlags::FAN_MOVED_FROM
        | MaskFlags::FAN_MOVED_TO
        | MaskFlags::FAN_CREATE
        | MaskFlags::FAN_DELETE
        | MaskFlags::FAN_DELETE_SELF
        | MaskFlags::FAN_MOVE_SELF
        | MaskFlags::FAN_RENAME
        | MaskFlags::FAN_Q_OVERFLOW
}

fn errno_to_io(error: Errno) -> io::Error {
    io::Error::from_raw_os_error(error as i32)
}

fn watcher_failed(
    message: impl Into<String>,
    source: impl std::fmt::Display,
) -> Report<WorkspaceWatcherError> {
    WorkspaceWatcherError::Failed(format!("{}: {source}", message.into())).report()
}
