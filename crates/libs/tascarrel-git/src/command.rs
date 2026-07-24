//! Bounded and redacted execution of the configured Git binary.

use std::collections::BTreeMap;
use std::env;
use std::ffi::OsString;
use std::path::Path;
use std::path::PathBuf;
use std::process::ExitStatus;
use std::process::Stdio;

use reportify::Report;
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt as _;
use tokio::process::Command;

use crate::GitError;
use crate::GitLimits;
use crate::GitResult;

const TRACE_ENVIRONMENT: [&str; 12] = [
    "GIT_CURL_VERBOSE",
    "GIT_TRACE",
    "GIT_TRACE_CURL",
    "GIT_TRACE_CURL_NO_DATA",
    "GIT_TRACE_PACKET",
    "GIT_TRACE_PACK_ACCESS",
    "GIT_TRACE_PERFORMANCE",
    "GIT_TRACE_SETUP",
    "GIT_TRACE_SHALLOW",
    "GIT_TRACE2",
    "GIT_TRACE2_EVENT",
    "GIT_TRACE2_PERF",
];

/// Configured system Git executable and process environment overrides.
#[derive(Clone)]
pub struct GitBinary {
    executable: PathBuf,
    environment: BTreeMap<OsString, Option<OsString>>,
}

impl std::fmt::Debug for GitBinary {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GitBinary")
            .field("executable", &self.executable)
            .field("environment_overrides", &self.environment.len())
            .finish()
    }
}

impl GitBinary {
    /// Discovers `git` from the current process search path.
    ///
    /// # Errors
    ///
    /// Returns [`GitError::GitNotFound`] when no regular executable file can
    /// be found, or an I/O report when a matching path cannot be resolved.
    pub fn discover() -> GitResult<Self> {
        let Some(search_path) = env::var_os("PATH") else {
            return Err(Report::new(GitError::GitNotFound));
        };
        for directory in env::split_paths(&search_path) {
            let candidate = directory.join("git");
            if !candidate.is_file() {
                continue;
            }
            let executable = candidate.canonicalize().map_err(|source| {
                Report::new(GitError::Io {
                    action: "resolve the Git executable",
                    source,
                })
            })?;
            return Self::new(executable);
        }
        Err(Report::new(GitError::GitNotFound))
    }

    /// Uses one explicit absolute Git executable.
    ///
    /// Inherited Git transport tracing is disabled by default because it can
    /// expose credential-bearing URLs and protocol headers. Callers can
    /// deliberately restore an individual variable with
    /// [`Self::with_environment`].
    ///
    /// # Errors
    ///
    /// Returns [`GitError::InvalidExecutable`] unless `executable` is an
    /// absolute regular file.
    pub fn new(executable: impl Into<PathBuf>) -> GitResult<Self> {
        let executable = executable.into();
        if !executable.is_absolute() || !executable.is_file() {
            return Err(Report::new(GitError::InvalidExecutable {
                path: executable,
            }));
        }
        Ok(Self {
            executable,
            environment: TRACE_ENVIRONMENT
                .into_iter()
                .map(|name| (OsString::from(name), None))
                .collect(),
        })
    }

    /// Overrides one environment variable for every managed Git subprocess.
    #[must_use]
    pub fn with_environment(
        mut self,
        name: impl Into<OsString>,
        value: impl Into<OsString>,
    ) -> Self {
        self.environment.insert(name.into(), Some(value.into()));
        self
    }

    /// Removes one inherited environment variable from every Git subprocess.
    #[must_use]
    pub fn without_environment(mut self, name: impl Into<OsString>) -> Self {
        self.environment.insert(name.into(), None);
        self
    }

    /// Returns the configured absolute executable path.
    #[must_use]
    pub fn executable(&self) -> &Path {
        &self.executable
    }

    /// Creates a Git command with the configured executable and environment.
    ///
    /// Daemon adapters can add platform-specific identity or transport setup
    /// before passing the command to [`crate::GitService`].
    #[must_use]
    pub fn command(&self) -> Command {
        let mut command = Command::new(&self.executable);
        for (name, value) in &self.environment {
            match value {
                Some(value) => {
                    command.env(name, value);
                }
                None => {
                    command.env_remove(name);
                }
            }
        }
        command
    }

    pub(crate) async fn run(
        &self,
        mut command: Command,
        action: &'static str,
        limits: &GitLimits,
        redactions: &[&str],
    ) -> GitResult<GitCommandOutput> {
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn().map_err(|source| {
            Report::new(GitError::Io {
                action: "start Git",
                source,
            })
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            Report::new(GitError::Io {
                action: "capture Git stdout",
                source: std::io::Error::other("Git stdout was not piped"),
            })
        })?;
        let stderr = child.stderr.take().ok_or_else(|| {
            Report::new(GitError::Io {
                action: "capture Git stderr",
                source: std::io::Error::other("Git stderr was not piped"),
            })
        })?;
        let (stdout, stderr, status) = tokio::join!(
            read_bounded(stdout, limits.command_output_bytes),
            read_bounded(stderr, limits.diagnostic_bytes),
            child.wait(),
        );
        let stdout = stdout.map_err(|source| {
            Report::new(GitError::Io {
                action: "read Git stdout",
                source,
            })
        })?;
        let stderr = stderr.map_err(|source| {
            Report::new(GitError::Io {
                action: "read Git stderr",
                source,
            })
        })?;
        let status = status.map_err(|source| {
            Report::new(GitError::Io {
                action: "wait for Git",
                source,
            })
        })?;
        if stdout.truncated {
            return Err(Report::new(GitError::OutputLimit {
                action,
                limit: limits.command_output_bytes,
            }));
        }
        Ok(GitCommandOutput {
            status,
            stdout: stdout.bytes,
            stderr: stderr.bytes,
            stderr_truncated: stderr.truncated,
            redactions: redactions.iter().map(ToString::to_string).collect(),
        })
    }
}

pub(crate) struct GitCommandOutput {
    pub(crate) status: ExitStatus,
    pub(crate) stdout: Vec<u8>,
    stderr: Vec<u8>,
    stderr_truncated: bool,
    redactions: Vec<String>,
}

impl GitCommandOutput {
    pub(crate) fn success(self, action: &'static str) -> GitResult<Vec<u8>> {
        if self.status.success() {
            return Ok(self.stdout);
        }
        Err(self.failure(action))
    }

    pub(crate) fn failure(&self, action: &'static str) -> Report<GitError> {
        let mut diagnostic = String::from_utf8_lossy(&self.stderr).trim().to_owned();
        for secret in &self.redactions {
            if !secret.is_empty() {
                diagnostic = diagnostic.replace(secret, "<remote>");
            }
        }
        if diagnostic.is_empty() {
            diagnostic = self.status.to_string();
        }
        if self.stderr_truncated {
            diagnostic.push_str(" [diagnostic truncated]");
        }
        Report::new(GitError::Command {
            action,
            status: self.status.code(),
            diagnostic,
        })
    }

    pub(crate) fn stdout_text(&self, action: &'static str) -> GitResult<&str> {
        std::str::from_utf8(&self.stdout)
            .map_err(|_| Report::new(GitError::MalformedOutput { action }))
    }
}

pub(crate) struct BoundedBytes {
    pub(crate) bytes: Vec<u8>,
    pub(crate) truncated: bool,
}

pub(crate) async fn read_bounded(
    mut reader: impl AsyncRead + Unpin,
    limit: usize,
) -> std::io::Result<BoundedBytes> {
    let mut bytes = Vec::with_capacity(limit.min(64 * 1024));
    let mut buffer = vec![0_u8; 16 * 1024];
    let mut truncated = false;
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        let remaining = limit.saturating_sub(bytes.len());
        if read <= remaining {
            bytes.extend_from_slice(&buffer[..read]);
        } else {
            bytes.extend_from_slice(&buffer[..remaining]);
            truncated = true;
        }
    }
    Ok(BoundedBytes { bytes, truncated })
}
