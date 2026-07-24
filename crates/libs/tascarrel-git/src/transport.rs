//! Streaming Git smart-protocol services over caller-owned transports.

use std::path::Path;
use std::process::ExitStatus;
use std::process::Stdio;

use reportify::Report;
use tokio::io::AsyncRead;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt as _;
use tokio::process::Child;
use tokio::process::ChildStdin;
use tokio::process::ChildStdout;

use crate::GitError;
use crate::GitResult;
use crate::ReceiveNamespace;
use crate::RepositoryStore;
use crate::command::read_bounded;

/// Running Git smart-protocol service ready to relay over a byte stream.
pub struct GitService {
    child: Child,
    stdin: ChildStdin,
    stdout: ChildStdout,
    diagnostic_bytes: usize,
    action: &'static str,
}

impl GitService {
    /// Starts `git upload-pack` from a caller-configured process command.
    ///
    /// The caller may configure operating-system identity, `safe.directory`,
    /// namespaces, or other daemon-specific process properties before this
    /// method adds the smart-protocol arguments and piped I/O.
    ///
    /// # Errors
    ///
    /// Returns a report when the diagnostic bound is zero or Git cannot be
    /// started with piped protocol I/O.
    pub fn start_upload_pack(
        command: tokio::process::Command,
        repository: &Path,
        diagnostic_bytes: usize,
    ) -> GitResult<Self> {
        Self::start(command, "upload-pack", repository, diagnostic_bytes)
    }

    /// Starts `git receive-pack` from a caller-configured process command.
    ///
    /// # Errors
    ///
    /// Returns a report when the diagnostic bound is zero or Git cannot be
    /// started with piped protocol I/O.
    pub fn start_receive_pack(
        command: tokio::process::Command,
        repository: &Path,
        diagnostic_bytes: usize,
    ) -> GitResult<Self> {
        Self::start(command, "receive-pack", repository, diagnostic_bytes)
    }

    fn start(
        mut command: tokio::process::Command,
        service: &'static str,
        repository: &Path,
        diagnostic_bytes: usize,
    ) -> GitResult<Self> {
        if diagnostic_bytes == 0 {
            return Err(Report::new(GitError::InvalidLimits));
        }
        command.arg(service);
        if service == "upload-pack" {
            command.arg("--strict");
        }
        command
            .arg(repository)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn().map_err(|source| {
            Report::new(GitError::Io {
                action: "start a Git smart-protocol service",
                source,
            })
        })?;
        let stdin = child.stdin.take().ok_or_else(|| {
            Report::new(GitError::Io {
                action: "open Git service stdin",
                source: std::io::Error::other("Git service stdin was not piped"),
            })
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            Report::new(GitError::Io {
                action: "open Git service stdout",
                source: std::io::Error::other("Git service stdout was not piped"),
            })
        })?;
        Ok(Self {
            child,
            stdin,
            stdout,
            diagnostic_bytes,
            action: if service == "upload-pack" {
                "serve upload-pack"
            } else {
                "serve receive-pack"
            },
        })
    }

    /// Relays the service over one full-duplex caller-owned stream.
    ///
    /// # Errors
    ///
    /// Returns a transport report when bytes cannot be relayed, or a bounded
    /// Git command report when the smart-protocol service fails.
    #[tracing::instrument(
        name = "tascarrel_git.service.relay",
        level = "debug",
        skip(self, stream),
        fields(service = self.action),
        err
    )]
    pub async fn relay<S>(mut self, stream: S) -> GitResult<()>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        self.relay_inner(stream, false).await.map(|_| ())
    }

    /// Relays the service and returns the still-open caller-owned stream.
    ///
    /// This lets a broker complete durable post-processing after the Git
    /// service exits and before the remote command observes connection close.
    ///
    /// # Errors
    ///
    /// Returns a transport report when bytes cannot be relayed, or a bounded
    /// Git command report when the smart-protocol service fails.
    #[tracing::instrument(
        name = "tascarrel_git.service.relay_retained",
        level = "debug",
        skip(self, stream),
        fields(service = self.action),
        err
    )]
    pub async fn relay_retained<S>(mut self, stream: S) -> GitResult<S>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        self.relay_inner(stream, true).await
    }

    async fn relay_inner<S>(&mut self, mut stream: S, retain_stream: bool) -> GitResult<S>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        let stderr = self.child.stderr.take().ok_or_else(|| {
            Report::new(GitError::Io {
                action: "capture Git service stderr",
                source: std::io::Error::other("Git service stderr was not piped"),
            })
        })?;
        let (mut stream_read, mut stream_write) = tokio::io::split(&mut stream);
        let to_git = async {
            tokio::io::copy(&mut stream_read, &mut self.stdin).await?;
            self.stdin.shutdown().await
        };
        let from_git = async {
            tokio::io::copy(&mut self.stdout, &mut stream_write).await?;
            if retain_stream {
                Ok(())
            } else {
                stream_write.shutdown().await
            }
        };
        let relay = async {
            let (to_git, from_git) = tokio::join!(to_git, from_git);
            to_git?;
            from_git
        };
        let (relay, diagnostic, status) = tokio::join!(
            relay,
            read_bounded(stderr, self.diagnostic_bytes),
            self.child.wait(),
        );
        relay.map_err(|source| {
            Report::new(GitError::Io {
                action: "relay the Git smart protocol",
                source,
            })
        })?;
        let diagnostic = diagnostic.map_err(|source| {
            Report::new(GitError::Io {
                action: "read Git service stderr",
                source,
            })
        })?;
        let status = status.map_err(|source| {
            Report::new(GitError::Io {
                action: "wait for the Git service",
                source,
            })
        })?;
        service_status(status, self.action, &diagnostic.bytes, diagnostic.truncated)?;
        drop(stream_read);
        drop(stream_write);
        Ok(stream)
    }
}

impl RepositoryStore {
    /// Starts `git upload-pack` for this store.
    ///
    /// The returned service is transport-agnostic. Hostd and guestd can relay
    /// it over a mux channel, while tests can use Unix or in-memory streams.
    ///
    /// # Errors
    ///
    /// Returns a report when Git cannot be started with piped protocol I/O.
    #[tracing::instrument(
        name = "tascarrel_git.store.upload_pack",
        level = "debug",
        skip(self),
        fields(repository = %self.path().display()),
        err
    )]
    pub fn upload_pack(&self) -> GitResult<GitService> {
        GitService::start_upload_pack(
            self.git().command(),
            self.path(),
            self.limits().diagnostic_bytes,
        )
    }

    /// Starts `git receive-pack` inside one isolated ref namespace.
    ///
    /// Ref deletion is rejected because managed publication currently retains
    /// an exact source object for every approval update.
    ///
    /// # Errors
    ///
    /// Returns a report when Git cannot be started with piped protocol I/O.
    #[tracing::instrument(
        name = "tascarrel_git.store.receive_pack",
        level = "debug",
        skip(self),
        fields(repository = %self.path().display(), namespace = %namespace),
        err
    )]
    pub fn receive_pack(&self, namespace: &ReceiveNamespace) -> GitResult<GitService> {
        let mut command = self.git().command();
        command
            .args(["-c", "receive.denyDeletes=true"])
            .env("GIT_NAMESPACE", namespace.as_str());
        GitService::start_receive_pack(command, self.path(), self.limits().diagnostic_bytes)
    }
}

fn service_status(
    status: ExitStatus,
    action: &'static str,
    diagnostic: &[u8],
    truncated: bool,
) -> GitResult<()> {
    if status.success() {
        return Ok(());
    }
    let mut diagnostic = String::from_utf8_lossy(diagnostic).trim().to_owned();
    if diagnostic.is_empty() {
        diagnostic = status.to_string();
    }
    if truncated {
        diagnostic.push_str(" [diagnostic truncated]");
    }
    Err(Report::new(GitError::Command {
        action,
        status: status.code(),
        diagnostic,
    }))
}
