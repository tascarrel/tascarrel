//! Pod-local materialization of one configured repository.
//!
//! [`run_repository_import`] clones through the authenticated Tascarrel Git
//! transport, configures the managed publication bridge, and publishes a
//! complete checkout without replacing an existing destination.

use std::ffi::OsString;
use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::io::Write as _;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use reportify::ErrorExt as _;
use reportify::ResultExt as _;
use rustix::fs::RenameFlags;
use tracing::warn;

use crate::error::PodctlError;
use crate::error::PodctlResult;
use crate::git::EXPECTED_CACHE_ID_ENVIRONMENT;
use crate::git::EXPECTED_CACHE_VERSION_ENVIRONMENT;
use crate::git::validate_repository_path;

const WORKSPACE_ROOT: &str = "/workspace";
const CHECKOUT_MARKER_FILE: &str = "tascarrel-cache.json";
const MAX_CHECKOUT_MARKER_BYTES: usize = 64 * 1024;
const MAX_COMMAND_DIAGNOSTIC_BYTES: usize = 8 * 1024;

/// Imports one exact configured repository version into the current pod.
#[tracing::instrument(level = "debug", skip_all, fields(path), err)]
pub(crate) fn run_repository_import(
    git: &Path,
    path: &str,
    branch: Option<&str>,
    cache_id: &str,
    cache_version: u64,
    encoded_marker: &str,
) -> PodctlResult<()> {
    let path = validate_repository_path(Path::new(path))?;
    if cache_id.is_empty() || cache_version == 0 {
        return Err(PodctlError::InvalidRepositoryCacheVersion.report());
    }
    let marker = STANDARD
        .decode(encoded_marker)
        .escalate(PodctlError::InvalidRepositoryImportMarker)?;
    if marker.is_empty() || marker.len() > MAX_CHECKOUT_MARKER_BYTES {
        return Err(PodctlError::InvalidRepositoryImportMarker.report());
    }

    let workspace = fs::canonicalize(WORKSPACE_ROOT).escalate(PodctlError::RepositoryImportIo {
        action: "resolve the pod workspace",
    })?;
    let destination = workspace.join(&path);
    match destination_state(&destination)? {
        DestinationState::Absent => {}
        DestinationState::Managed => return print_outcome("already-present"),
        DestinationState::Occupied => return print_outcome("destination-occupied"),
    }

    let parent = destination
        .parent()
        .ok_or_else(|| PodctlError::InvalidRepositoryPath.report())?;
    fs::create_dir_all(parent).escalate(PodctlError::RepositoryImportIo {
        action: "create the repository parent directory",
    })?;
    let parent = fs::canonicalize(parent).escalate(PodctlError::RepositoryImportIo {
        action: "resolve the repository parent directory",
    })?;
    if !parent.starts_with(&workspace) {
        return Err(PodctlError::InvalidRepositoryPath.report());
    }

    let staging_name = format!(".tascarrel-import-{}", uuid::Uuid::new_v4());
    let staging = parent.join(&staging_name);
    let mut cleanup = StagingCleanup(Some(staging.clone()));
    clone_repository(git, &path, branch, cache_id, cache_version, &staging)?;
    configure_repository(git, &path, &staging)?;
    write_checkout_marker(&staging, &marker)?;

    let parent_directory = File::open(&parent).escalate(PodctlError::RepositoryImportIo {
        action: "open the repository parent directory",
    })?;
    let destination_name = destination
        .file_name()
        .ok_or_else(|| PodctlError::InvalidRepositoryPath.report())?;
    match rustix::fs::renameat_with(
        &parent_directory,
        &staging_name,
        &parent_directory,
        destination_name,
        RenameFlags::NOREPLACE,
    ) {
        Ok(()) => {
            cleanup.0 = None;
            parent_directory
                .sync_all()
                .escalate(PodctlError::RepositoryImportIo {
                    action: "synchronize the imported repository",
                })?;
            print_outcome("imported")
        }
        Err(error) if error == rustix::io::Errno::EXIST => print_outcome("destination-occupied"),
        Err(error) => Err(io::Error::from(error)).escalate(PodctlError::RepositoryImportIo {
            action: "publish the imported repository",
        }),
    }
}

/// Classifies an import destination without following its final components.
fn destination_state(destination: &Path) -> PodctlResult<DestinationState> {
    let metadata = match fs::symlink_metadata(destination) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(DestinationState::Absent);
        }
        Err(error) => {
            return Err(error).escalate(PodctlError::RepositoryImportIo {
                action: "inspect the repository destination",
            });
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Ok(DestinationState::Occupied);
    }
    let git_directory = destination.join(".git");
    let git_directory = match fs::symlink_metadata(&git_directory) {
        Ok(git_directory) => git_directory,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(DestinationState::Occupied);
        }
        Err(error) => {
            return Err(error).escalate(PodctlError::RepositoryImportIo {
                action: "inspect the existing Git directory",
            });
        }
    };
    if git_directory.file_type().is_symlink() || !git_directory.is_dir() {
        return Ok(DestinationState::Occupied);
    }
    let marker = destination.join(".git").join(CHECKOUT_MARKER_FILE);
    let marker = match fs::symlink_metadata(marker) {
        Ok(marker) => marker,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(DestinationState::Occupied);
        }
        Err(error) => {
            return Err(error).escalate(PodctlError::RepositoryImportIo {
                action: "inspect the existing checkout marker",
            });
        }
    };
    if marker.is_file() && !marker.file_type().is_symlink() {
        Ok(DestinationState::Managed)
    } else {
        Ok(DestinationState::Occupied)
    }
}

/// Clones one pinned cache version into an unpublished staging directory.
fn clone_repository(
    git: &Path,
    path: &str,
    branch: Option<&str>,
    cache_id: &str,
    cache_version: u64,
    staging: &Path,
) -> PodctlResult<()> {
    let mut command = git_command(git);
    command.args(["clone", "--no-hardlinks"]);
    if let Some(branch) = branch {
        command.args(["--branch", branch]);
    }
    command
        .arg(format!("tascarrel://workspace/{path}"))
        .arg(staging)
        .env(EXPECTED_CACHE_ID_ENVIRONMENT, cache_id)
        .env(
            EXPECTED_CACHE_VERSION_ENVIRONMENT,
            cache_version.to_string(),
        );
    run_git_command(&mut command, "clone the configured repository")
}

/// Configures mediated fetch and publication remotes in a staged checkout.
fn configure_repository(git: &Path, path: &str, staging: &Path) -> PodctlResult<()> {
    let origin = format!("tascarrel://workspace/{path}");
    let push_origin = format!("file:///workspace/{path}");
    for (key, value, action) in [
        (
            "remote.origin.url",
            origin.as_str(),
            "configure the repository fetch origin",
        ),
        (
            "remote.origin.pushurl",
            push_origin.as_str(),
            "configure the repository publication origin",
        ),
        (
            "remote.origin.receivepack",
            "/usr/local/bin/tascarrel-git-receive-pack",
            "configure the repository publication bridge",
        ),
    ] {
        let mut command = git_command(git);
        command
            .arg("-C")
            .arg(staging)
            .args(["config", "--local", key, value]);
        run_git_command(&mut command, action)?;
    }
    Ok(())
}

/// Creates a pinned Git command which can find Tascarrel's injected helper.
fn git_command(git: &Path) -> Command {
    let mut path = OsString::from("/usr/local/bin");
    if let Some(inherited) = std::env::var_os("PATH")
        && !inherited.is_empty()
    {
        path.push(":");
        path.push(inherited);
    }
    let mut command = Command::new(git);
    command.env("PATH", path);
    command
}

/// Runs one Git command and retains only a bounded display-safe diagnostic.
fn run_git_command(command: &mut Command, action: &'static str) -> PodctlResult<()> {
    let output = command
        .output()
        .escalate(PodctlError::RepositoryImportIo { action })?;
    if output.status.success() {
        Ok(())
    } else {
        Err(PodctlError::RepositoryImportCommand {
            action,
            detail: bounded_diagnostic(&output.stderr),
        }
        .report())
    }
}

/// Durably marks one staged checkout as managed by Tascarrel.
fn write_checkout_marker(checkout: &Path, marker: &[u8]) -> PodctlResult<()> {
    let git_directory = checkout.join(".git");
    let metadata =
        fs::symlink_metadata(&git_directory).escalate(PodctlError::RepositoryImportIo {
            action: "inspect the imported Git directory",
        })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(PodctlError::InvalidRepositoryImportMarker.report());
    }
    let marker_path = git_directory.join(CHECKOUT_MARKER_FILE);
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&marker_path)
        .escalate(PodctlError::RepositoryImportIo {
            action: "create the imported checkout marker",
        })?;
    file.write_all(marker)
        .escalate(PodctlError::RepositoryImportIo {
            action: "write the imported checkout marker",
        })?;
    file.sync_all().escalate(PodctlError::RepositoryImportIo {
        action: "synchronize the imported checkout marker",
    })?;
    File::open(git_directory)
        .and_then(|directory| directory.sync_all())
        .escalate(PodctlError::RepositoryImportIo {
            action: "synchronize the imported Git directory",
        })
}

/// Bounds and sanitizes a command diagnostic for an error report.
fn bounded_diagnostic(bytes: &[u8]) -> String {
    let bytes = &bytes[..bytes.len().min(MAX_COMMAND_DIAGNOSTIC_BYTES)];
    let mut diagnostic = String::with_capacity(bytes.len());
    for character in String::from_utf8_lossy(bytes).chars() {
        let character = if character == '\n' || character == '\t' || !character.is_control() {
            character
        } else {
            '�'
        };
        if diagnostic.len() + character.len_utf8() > MAX_COMMAND_DIAGNOSTIC_BYTES {
            break;
        }
        diagnostic.push(character);
    }
    diagnostic.trim().to_owned()
}

/// Writes the machine-readable helper outcome.
fn print_outcome(outcome: &str) -> PodctlResult<()> {
    writeln!(io::stdout().lock(), "{outcome}").escalate(PodctlError::WriteOutput)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// Existing state of one configured repository destination.
enum DestinationState {
    Absent,
    Managed,
    Occupied,
}

/// Removes an unpublished staging checkout when the import exits early.
struct StagingCleanup(Option<PathBuf>);

impl Drop for StagingCleanup {
    fn drop(&mut self) {
        let Some(staging) = self.0.take() else {
            return;
        };
        if let Err(error) = fs::remove_dir_all(&staging)
            && error.kind() != io::ErrorKind::NotFound
        {
            warn!(path = %staging.display(), %error, "could not remove repository import staging directory");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use super::*;

    /// Distinguishes absent, managed, and unmanaged import destinations.
    #[test]
    fn destination_state_never_treats_unmanaged_content_as_importable() {
        let temporary = tempfile::tempdir().unwrap();
        let absent = temporary.path().join("absent");
        assert_eq!(
            destination_state(&absent).unwrap(),
            DestinationState::Absent
        );

        let occupied = temporary.path().join("occupied");
        fs::create_dir(&occupied).unwrap();
        assert_eq!(
            destination_state(&occupied).unwrap(),
            DestinationState::Occupied
        );

        let managed = temporary.path().join("managed");
        fs::create_dir_all(managed.join(".git")).unwrap();
        fs::write(managed.join(".git").join(CHECKOUT_MARKER_FILE), b"fixture").unwrap();
        assert_eq!(
            destination_state(&managed).unwrap(),
            DestinationState::Managed
        );
    }

    /// Refuses symlink destinations and symlinked management markers.
    #[test]
    fn destination_state_rejects_symlinks() {
        let temporary = tempfile::tempdir().unwrap();
        let target = temporary.path().join("target");
        fs::create_dir(&target).unwrap();
        let destination = temporary.path().join("destination");
        symlink(&target, &destination).unwrap();
        assert_eq!(
            destination_state(&destination).unwrap(),
            DestinationState::Occupied
        );

        let checkout = temporary.path().join("symlinked-git");
        fs::create_dir(&checkout).unwrap();
        let git_directory = temporary.path().join("git-directory");
        fs::create_dir(&git_directory).unwrap();
        fs::write(git_directory.join(CHECKOUT_MARKER_FILE), b"fixture").unwrap();
        symlink(&git_directory, checkout.join(".git")).unwrap();
        assert_eq!(
            destination_state(&checkout).unwrap(),
            DestinationState::Occupied
        );

        let checkout = temporary.path().join("checkout");
        fs::create_dir_all(checkout.join(".git")).unwrap();
        let marker_target = temporary.path().join("marker");
        fs::write(&marker_target, b"fixture").unwrap();
        symlink(
            &marker_target,
            checkout.join(".git").join(CHECKOUT_MARKER_FILE),
        )
        .unwrap();
        assert_eq!(
            destination_state(&checkout).unwrap(),
            DestinationState::Occupied
        );
    }
}
