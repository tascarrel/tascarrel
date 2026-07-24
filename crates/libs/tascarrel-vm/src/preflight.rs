//! Executable discovery and version reporting before VM startup.
//!
//! [`preflight`] resolves the programs needed by a [`crate::VmConfig`], probes
//! their version interfaces, and selects the usable shared-directory transport.

use std::env;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;
use tracing::debug;

use crate::SharedDirectoryTransport;
use crate::VmConfig;

/// Result of checking the host executables needed by a VM configuration.
#[derive(Clone, Debug, Eq, PartialEq)]
#[must_use]
pub struct PreflightReport {
    qemu: ExecutablePreflightReport,
    virtiofsd: Option<ExecutablePreflightReport>,
    shared_directory_transport: SharedDirectoryTransport,
}

impl PreflightReport {
    /// Returns the QEMU executable check.
    pub const fn qemu(&self) -> &ExecutablePreflightReport {
        &self.qemu
    }

    /// Returns the virtiofsd check when that backend was needed on Linux.
    #[must_use]
    pub const fn virtiofsd(&self) -> Option<&ExecutablePreflightReport> {
        self.virtiofsd.as_ref()
    }

    /// Returns the transport selected from the available host executables.
    #[must_use]
    pub const fn shared_directory_transport(&self) -> SharedDirectoryTransport {
        self.shared_directory_transport
    }
}

/// Discovery and version result for one configured executable.
#[derive(Clone, Debug, Eq, PartialEq)]
#[must_use]
pub struct ExecutablePreflightReport {
    requested_path: PathBuf,
    resolved_path: Option<PathBuf>,
    version: Option<String>,
    failure: Option<String>,
}

impl ExecutablePreflightReport {
    /// Returns the path or program name supplied by configuration.
    #[must_use]
    pub fn requested_path(&self) -> &Path {
        &self.requested_path
    }

    /// Returns the absolute executable path when discovery succeeded.
    #[must_use]
    pub fn resolved_path(&self) -> Option<&Path> {
        self.resolved_path.as_deref()
    }

    /// Returns the first non-empty line reported by `--version`.
    #[must_use]
    pub fn version(&self) -> Option<&str> {
        self.version.as_deref()
    }

    /// Returns why discovery or version probing failed.
    #[must_use]
    pub fn failure(&self) -> Option<&str> {
        self.failure.as_deref()
    }

    /// Reports whether the executable was resolved and returned a version.
    #[must_use]
    pub const fn is_available(&self) -> bool {
        self.resolved_path.is_some() && self.version.is_some()
    }

    fn available(requested_path: PathBuf, resolved_path: PathBuf, version: String) -> Self {
        Self {
            requested_path,
            resolved_path: Some(resolved_path),
            version: Some(version),
            failure: None,
        }
    }

    fn unavailable(
        requested_path: PathBuf,
        resolved_path: Option<PathBuf>,
        failure: String,
    ) -> Self {
        Self {
            requested_path,
            resolved_path,
            version: None,
            failure: Some(failure),
        }
    }
}

/// Resolves and probes every host executable needed by `config`.
///
/// Missing or unusable QEMU is represented in the returned report and prevents
/// [`crate::Vm::spawn`] from starting. On Linux, an unavailable virtiofsd
/// selects [`SharedDirectoryTransport::Virtio9p`].
#[tracing::instrument(
    name = "tascarrel_vm.preflight",
    level = "debug",
    skip(config),
    fields(
        qemu = %config.qemu_binary().display(),
        virtiofsd = %config.virtiofsd_binary().display(),
        shared_directories = config.shared_directories().len(),
        configured_transport = ?config.shared_directory_transport(),
    ),
    ret
)]
pub async fn preflight(config: &VmConfig) -> PreflightReport {
    let qemu = inspect_executable(config.qemu_binary(), config.startup_timeout());
    let needs_virtiofsd = !config.shared_directories().is_empty()
        && config.shared_directory_transport() == SharedDirectoryTransport::Virtiofs;
    if needs_virtiofsd {
        let virtiofsd = inspect_executable(config.virtiofsd_binary(), config.startup_timeout());
        let (qemu, virtiofsd) = tokio::join!(qemu, virtiofsd);
        let shared_directory_transport = if virtiofsd.is_available() {
            SharedDirectoryTransport::Virtiofs
        } else {
            SharedDirectoryTransport::Virtio9p
        };
        PreflightReport {
            qemu,
            virtiofsd: Some(virtiofsd),
            shared_directory_transport,
        }
    } else {
        PreflightReport {
            qemu: qemu.await,
            virtiofsd: None,
            shared_directory_transport: config.shared_directory_transport(),
        }
    }
}

/// Resolves one executable and obtains its first version-output line.
#[tracing::instrument(
    name = "tascarrel_vm.preflight.executable",
    level = "debug",
    skip_all,
    fields(program = %program.display(), ?timeout),
    ret
)]
async fn inspect_executable(program: &Path, timeout: Duration) -> ExecutablePreflightReport {
    let requested_path = program.to_owned();
    let resolved_path = match resolve_executable(program) {
        ExecutableResolution::Available(path) => path,
        ExecutableResolution::Unavailable(failure) => {
            return ExecutablePreflightReport::unavailable(requested_path, None, failure);
        }
    };
    let mut command = Command::new(&resolved_path);
    command
        .arg("--version")
        .stdin(Stdio::null())
        .kill_on_drop(true);
    let output = match tokio::time::timeout(timeout, command.output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => {
            return ExecutablePreflightReport::unavailable(
                requested_path,
                Some(resolved_path.clone()),
                format!(
                    "failed to run {} --version: {error}",
                    resolved_path.display()
                ),
            );
        }
        Err(_) => {
            return ExecutablePreflightReport::unavailable(
                requested_path,
                Some(resolved_path.clone()),
                format!(
                    "{} --version did not finish within {timeout:?}",
                    resolved_path.display()
                ),
            );
        }
    };
    if !output.status.success() {
        return ExecutablePreflightReport::unavailable(
            requested_path,
            Some(resolved_path.clone()),
            format!(
                "{} --version exited with {}",
                resolved_path.display(),
                output.status
            ),
        );
    }
    let version =
        first_non_empty_line(&output.stdout).or_else(|| first_non_empty_line(&output.stderr));
    match version {
        Some(version) => {
            ExecutablePreflightReport::available(requested_path, resolved_path, version)
        }
        None => ExecutablePreflightReport::unavailable(
            requested_path,
            Some(resolved_path.clone()),
            format!(
                "{} --version returned no version text",
                resolved_path.display()
            ),
        ),
    }
}

/// Resolves a direct path or searches an executable program name in `PATH`.
fn resolve_executable(program: &Path) -> ExecutableResolution {
    if program.components().count() != 1 {
        return match executable_candidate(program) {
            ExecutableCandidate::Available(path) => ExecutableResolution::Available(path),
            ExecutableCandidate::Absent => ExecutableResolution::Unavailable(format!(
                "configured executable does not exist: {}",
                program.display()
            )),
            ExecutableCandidate::Unusable(reason) => ExecutableResolution::Unavailable(format!(
                "configured executable is unusable: {reason}"
            )),
        };
    }
    let Some(search_path) = env::var_os("PATH") else {
        return ExecutableResolution::Unavailable(format!(
            "PATH is unset while resolving {}",
            program.display()
        ));
    };
    let mut first_unusable = None;
    for directory in env::split_paths(&search_path) {
        match executable_candidate(&directory.join(program)) {
            ExecutableCandidate::Available(path) => {
                return ExecutableResolution::Available(path);
            }
            ExecutableCandidate::Absent => {}
            ExecutableCandidate::Unusable(reason) => {
                first_unusable.get_or_insert(reason);
            }
        }
    }
    match first_unusable {
        Some(reason) => ExecutableResolution::Unavailable(format!(
            "no usable executable named {} was found in PATH; {reason}",
            program.display()
        )),
        None => ExecutableResolution::Unavailable(format!(
            "executable {} was not found in PATH",
            program.display()
        )),
    }
}

/// Classifies one direct executable candidate without discarding I/O failures.
fn executable_candidate(path: &Path) -> ExecutableCandidate {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return ExecutableCandidate::Absent;
        }
        Err(error) => {
            debug!(path = %path.display(), %error, "failed to inspect executable candidate");
            return ExecutableCandidate::Unusable(format!(
                "failed to inspect {}: {error}",
                path.display()
            ));
        }
    };
    if !metadata.is_file() {
        return ExecutableCandidate::Unusable(format!("{} is not a regular file", path.display()));
    }
    match fs::canonicalize(path) {
        Ok(path) => ExecutableCandidate::Available(path),
        Err(error) => {
            debug!(path = %path.display(), %error, "failed to resolve executable candidate");
            ExecutableCandidate::Unusable(format!("failed to resolve {}: {error}", path.display()))
        }
    }
}

/// Extracts one concise version line from command output.
fn first_non_empty_line(output: &[u8]) -> Option<String> {
    String::from_utf8_lossy(output)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(ToOwned::to_owned)
}

/// Final result of resolving a configured executable.
enum ExecutableResolution {
    Available(PathBuf),
    Unavailable(String),
}

/// Classification of one direct executable candidate.
enum ExecutableCandidate {
    Available(PathBuf),
    Absent,
    Unusable(String),
}
