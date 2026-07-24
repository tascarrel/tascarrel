//! Managed Git transport adapters over the pod's multiplexed guestd socket.
//!
//! The adapters implement Git's remote-helper and receive-pack process
//! protocols while keeping repository paths scoped below `/workspace`.

use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::io::Write as _;
use std::net::Shutdown;
use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::Path;
use std::str::FromStr as _;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::thread;

use reportify::ErrorExt as _;
use reportify::ResultExt as _;
use tascarrel_api::types::repositories;
use tascarrel_protocol::Framed;
use tascarrel_protocol::MUX_POD_GIT_ENDPOINT;
use tascarrel_protocol::PodGitRequest;
use tascarrel_protocol::PodGitService;
use tokio::net::UnixStream;
use tokio::task::JoinHandle;

use crate::CONTROL_SOCKET;
use crate::client::PodClient;
use crate::error::PodctlError;
use crate::error::PodctlResult;

/// Implements Git's blocking remote-helper protocol for managed fetches.
#[tracing::instrument(level = "debug", skip_all, err)]
pub(crate) fn run_git_remote_helper() -> PodctlResult<()> {
    let remote = std::env::args()
        .nth(2)
        .ok_or_else(|| PodctlError::InvalidGitInvocation.report())?;
    let path = remote
        .strip_prefix("tascarrel://workspace/")
        .ok_or_else(|| PodctlError::InvalidGitRemote.report())?;
    let path = validate_repository_path(Path::new(path))?;
    let input =
        File::open("/dev/stdin").escalate(PodctlError::OpenGitStream { stream: "input" })?;
    let output = OpenOptions::new()
        .write(true)
        .open("/dev/stdout")
        .escalate(PodctlError::OpenGitStream { stream: "output" })?;
    tascarrel_git::run_remote_helper(input, output, |service| {
        if service != tascarrel_git::RemoteService::UploadPack {
            return Err(reportify::Report::new(
                tascarrel_git::GitError::UnsupportedService {
                    service: "git-receive-pack".to_owned(),
                },
            ));
        }
        connect_blocking_git(path, PodGitService::UploadPack).map_err(git_helper_io)
    })
    .map_err(|error| error.escalate(PodctlError::GitHelper))
}

/// Implements the local receive-pack program configured for managed pushes.
#[tracing::instrument(level = "debug", skip_all, err)]
pub(crate) async fn run_git_receive_pack() -> PodctlResult<()> {
    let path = std::env::args_os()
        .nth(1)
        .ok_or_else(|| PodctlError::InvalidGitInvocation.report())?;
    let relative = Path::new(&path)
        .strip_prefix("/workspace")
        .escalate(PodctlError::InvalidRepositoryPath)?;
    let path = validate_repository_path(relative)?;
    let (mut channel, _session, response) =
        open_git_channel(path, PodGitService::ReceivePack).await?;
    let push_id = match response {
        tascarrel_protocol::GitOpenResponse::ReceivePackReady { push_id } => push_id,
        tascarrel_protocol::GitOpenResponse::Error { error } => {
            return Err(PodctlError::GitRejected(error).report());
        }
        tascarrel_protocol::GitOpenResponse::Ready
        | tascarrel_protocol::GitOpenResponse::VersionedReady { .. } => {
            return Err(PodctlError::InvalidGitResponse.report());
        }
    };
    let (mut read, mut write) = tokio::io::split(&mut channel);
    let mut input = tokio::io::stdin();
    let mut output = tokio::io::stdout();
    let to_guest = async {
        tokio::io::copy(&mut input, &mut write).await?;
        tokio::io::AsyncWriteExt::shutdown(&mut write).await
    };
    let from_guest = async {
        tokio::io::copy(&mut read, &mut output).await?;
        tokio::io::AsyncWriteExt::flush(&mut output).await
    };
    tokio::try_join!(to_guest, from_guest).escalate(PodctlError::RelayReceivePack)?;
    drop(read);
    drop(write);
    channel
        .close()
        .await
        .map_err(|error| error.escalate(PodctlError::CloseGitChannel))?;

    let client = PodClient::connect(Path::new(CONTROL_SOCKET)).await?;
    let push_id = repositories::RepositoryPushId::from_str(&push_id)
        .escalate(PodctlError::InvalidGitResponse)?;
    let mut cursor = None;
    loop {
        let event = client
            .first_host_event(repositories::RepositoryPushStatusChangedSubscription {
                workspace: client.identity().workspace.clone(),
                pod_id: client.identity().pod_id.clone(),
                push_id: push_id.clone(),
                cursor,
            })
            .await?;
        cursor = Some(event.revision);
        match event.value {
            repositories::RepositoryPushStatus::Published => break,
            repositories::RepositoryPushStatus::ApprovalRequired(approval_id) => {
                writeln!(
                    io::stderr().lock(),
                    "Tascarrel repository approval required: {}; waiting for publication",
                    approval_id.0
                )
                .escalate(PodctlError::WriteOutput)?;
            }
            repositories::RepositoryPushStatus::Denied(message) => {
                return Err(PodctlError::GitPushDenied(message.to_string()).report());
            }
            repositories::RepositoryPushStatus::Rejected => {
                return Err(PodctlError::GitPushRejected.report());
            }
            repositories::RepositoryPushStatus::Failed(message) => {
                return Err(PodctlError::GitPushFailed(message.to_string()).report());
            }
        }
    }
    Ok(())
}

/// Adapts the asynchronous mux channel to Git's required blocking interface.
fn connect_blocking_git(
    path: String,
    service: PodGitService,
) -> io::Result<(GitBridgeReader, StdUnixStream)> {
    let (helper, bridge) = StdUnixStream::pair()?;
    let (ready, accepted) = std::sync::mpsc::sync_channel(1);
    let (finished, completion) = std::sync::mpsc::sync_channel(1);
    let bridge_ready = Arc::new(AtomicBool::new(false));
    thread::spawn(move || {
        let failed = ready.clone();
        let relay_started = Arc::clone(&bridge_ready);
        let result = runtime().and_then(|runtime| {
            runtime.block_on(async move {
                let (mut channel, _session, response) = open_git_channel(path, service).await?;
                match (service, response) {
                    (PodGitService::UploadPack, tascarrel_protocol::GitOpenResponse::Ready)
                    | (
                        PodGitService::ReceivePack,
                        tascarrel_protocol::GitOpenResponse::ReceivePackReady { .. },
                    ) => {}
                    (_, tascarrel_protocol::GitOpenResponse::Error { error }) => {
                        return Err(PodctlError::GitRejected(error).report());
                    }
                    _ => return Err(PodctlError::InvalidGitResponse.report()),
                }
                bridge
                    .set_nonblocking(true)
                    .escalate(PodctlError::PrepareGitStream)?;
                let mut bridge = tokio::net::UnixStream::from_std(bridge)
                    .escalate(PodctlError::PrepareGitStream)?;
                ready
                    .send(Ok(()))
                    .map_err(|_| PodctlError::GitHelperStopped.report())?;
                relay_started.store(true, Ordering::Release);
                relay_git_stream(&mut channel, &mut bridge).await
            })
        });
        let already_started = bridge_ready.load(Ordering::Acquire);
        match result {
            Ok(()) if already_started => {
                if finished.send(Ok(())).is_err() {
                    tracing::debug!("Git helper stopped before bridge completion was reported");
                }
            }
            Ok(()) => {}
            Err(error) if already_started => {
                tracing::error!(error = ?error, "pod Git bridge failed after startup");
                if finished
                    .send(Err(io::Error::other(error.to_string())))
                    .is_err()
                {
                    tracing::debug!("Git helper stopped before bridge failure was reported");
                }
            }
            Err(error) => {
                if failed
                    .send(Err(io::Error::other(error.to_string())))
                    .is_err()
                {
                    tracing::error!(error = ?error, "pod Git bridge failed before startup");
                }
            }
        }
    });
    accepted
        .recv()
        .map_err(|_| io::Error::other(PodctlError::GitBridgeStopped.to_string()))??;
    let reader = helper.try_clone()?;
    Ok((
        GitBridgeReader {
            stream: reader,
            completion: Some(completion),
        },
        helper,
    ))
}

/// Blocking Git reader which keeps its asynchronous mux bridge alive through
/// the channel close handshake.
struct GitBridgeReader {
    stream: StdUnixStream,
    completion: Option<std::sync::mpsc::Receiver<io::Result<()>>>,
}

impl GitBridgeReader {
    fn finish(&mut self) -> io::Result<()> {
        let Some(completion) = self.completion.take() else {
            return Ok(());
        };
        let shutdown = self.stream.shutdown(Shutdown::Both);
        let completed = completion
            .recv()
            .map_err(|_| io::Error::other(PodctlError::GitBridgeStopped.to_string()))?;
        shutdown?;
        completed
    }
}

impl io::Read for GitBridgeReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let read = self.stream.read(buffer)?;
        if read == 0 {
            self.finish()?;
        }
        Ok(read)
    }
}

impl Drop for GitBridgeReader {
    fn drop(&mut self) {
        if self.completion.take().is_some()
            && let Err(error) = self.stream.shutdown(Shutdown::Both)
        {
            tracing::warn!(%error, "pod Git bridge did not close cleanly");
        }
    }
}

/// Creates the runtime used by the blocking Git bridge thread.
fn runtime() -> PodctlResult<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .escalate(PodctlError::Runtime)
}

/// Owns the background multiplexer resources for one Git channel.
struct GitSession {
    _incoming: tascarrel_mux::Incoming,
    driver: JoinHandle<tascarrel_mux::Result<()>>,
}

impl Drop for GitSession {
    fn drop(&mut self) {
        self.driver.abort();
    }
}

/// Opens one authenticated raw Git channel and validates its handshake.
#[tracing::instrument(level = "debug", skip_all, err)]
async fn open_git_channel(
    path: String,
    service: PodGitService,
) -> PodctlResult<(
    tascarrel_mux::Channel,
    GitSession,
    tascarrel_protocol::GitOpenResponse,
)> {
    let stream = UnixStream::connect(CONTROL_SOCKET)
        .await
        .escalate(PodctlError::ConnectControlSocket)?;
    let (driver, mux, incoming) = tascarrel_mux::connect(
        stream,
        tascarrel_mux::Role::Client,
        tascarrel_mux::Config::default(),
    )
    .map_err(|error| error.escalate(PodctlError::Multiplexer))?;
    let session = GitSession {
        _incoming: incoming,
        driver: tokio::spawn(driver.run()),
    };
    let channel = mux
        .open(MUX_POD_GIT_ENDPOINT)
        .await
        .map_err(|error| error.escalate(PodctlError::Multiplexer))?;
    let mut framed = Framed::new(channel);
    framed
        .write(&PodGitRequest { path, service })
        .await
        .map_err(|error| error.escalate(PodctlError::GitHandshake))?;
    let response = framed
        .read::<tascarrel_protocol::GitOpenResponse>()
        .await
        .map_err(|error| error.escalate(PodctlError::GitHandshake))?
        .ok_or_else(|| PodctlError::GitConnectionClosed.report())?;
    Ok((framed.into_inner(), session, response))
}

/// Relays Git bytes until the service closes its response stream.
#[tracing::instrument(level = "debug", skip_all, err)]
async fn relay_git_stream(
    channel: &mut tascarrel_mux::Channel,
    bridge: &mut tokio::net::UnixStream,
) -> PodctlResult<()> {
    let (mut channel_read, mut channel_write) = tokio::io::split(&mut *channel);
    let (mut bridge_read, mut bridge_write) = bridge.split();
    let to_guest = async {
        tokio::io::copy(&mut bridge_read, &mut channel_write).await?;
        tokio::io::AsyncWriteExt::shutdown(&mut channel_write).await
    };
    let from_guest = async {
        tokio::io::copy(&mut channel_read, &mut bridge_write).await?;
        tokio::io::AsyncWriteExt::shutdown(&mut bridge_write).await
    };
    let (request, response) = tokio::join!(to_guest, from_guest);
    request.escalate(PodctlError::RelayGitRequest)?;
    response.escalate(PodctlError::RelayGitResponse)?;
    drop(channel_read);
    drop(channel_write);
    channel
        .close()
        .await
        .map_err(|error| error.escalate(PodctlError::CloseGitChannel))?;
    Ok(())
}

/// Validates a configured path relative to the workspace root.
fn validate_repository_path(path: &Path) -> PodctlResult<String> {
    if path.as_os_str().is_empty()
        || path
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err(PodctlError::InvalidRepositoryPath.report());
    }
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| PodctlError::InvalidRepositoryPath.report())
}

/// Converts a blocking bridge failure to tascarrel-git's I/O category.
fn git_helper_io(source: io::Error) -> reportify::Report<tascarrel_git::GitError> {
    reportify::Report::new(tascarrel_git::GitError::Io {
        action: "connect the Tascarrel Git helper",
        source,
    })
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::validate_repository_path;

    /// Verifies repository arguments cannot address paths outside the managed
    /// workspace tree.
    #[test]
    fn git_repository_paths_cannot_escape_workspace() {
        assert_eq!(
            validate_repository_path(Path::new("src/tascarrel")).unwrap(),
            "src/tascarrel"
        );
        for path in ["", "/src/tascarrel", "../tascarrel", "src/../tascarrel"] {
            assert!(
                validate_repository_path(Path::new(path)).is_err(),
                "unexpectedly accepted {path:?}"
            );
        }
    }
}
