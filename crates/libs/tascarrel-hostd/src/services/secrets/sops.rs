//! SOPS subprocess integration with bounded in-memory plaintext handling.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs::OpenOptions;
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use reportify::Report;
use sha2::Digest as _;
use sha2::Sha256;
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt as _;
use tokio::io::AsyncWriteExt as _;
use tokio::process::Command;
use tokio::time::timeout;
use tracing::debug;
use uuid::Uuid;

use super::service::SecretsServiceError;

/// One SOPS provider bound to a validated workspace directory and encrypted
/// JSON file.
pub(crate) struct SopsProvider {
    workspace_directory: PathBuf,
    relative_file: PathBuf,
    sops_executable: PathBuf,
    command_timeout: Duration,
    max_document_bytes: u64,
}

impl SopsProvider {
    /// Validates and binds a configured provider instance.
    pub(crate) fn new(
        workspace_directory: PathBuf,
        configured_file: Option<&str>,
        sops_executable: PathBuf,
        command_timeout: Duration,
        max_document_bytes: u64,
    ) -> Result<Self, Report<SecretsServiceError>> {
        let relative_file = validate_relative_file(configured_file.unwrap_or("secrets.json"))?;
        validate_workspace_directory(&workspace_directory)?;
        validate_parent_directories(&workspace_directory, &relative_file)?;
        validate_secret_file(&workspace_directory.join(&relative_file))?;
        Ok(Self {
            workspace_directory,
            relative_file,
            sops_executable,
            command_timeout,
            max_document_bytes,
        })
    }

    /// Decrypts the provider document, returning an empty snapshot when it does
    /// not exist.
    pub(crate) async fn load(&self) -> Result<SopsSnapshot, Report<SecretsServiceError>> {
        let path = self.workspace_directory.join(&self.relative_file);
        validate_secret_file(&path)?;
        let encrypted = match read_bounded_file(&path, self.max_document_bytes).await {
            Ok(Some(encrypted)) => encrypted,
            Ok(None) => return Ok(SopsSnapshot::empty()),
            Err(report) => return Err(report),
        };
        let revision = revision(&encrypted);
        let plaintext = self.run(SopsOperation::Decrypt, encrypted).await?;
        let values: BTreeMap<String, String> =
            serde_json::from_slice(&plaintext).map_err(|error| {
                SecretsServiceError::unavailable(
                    "SOPS plaintext is not a string-valued JSON object",
                )
                .message(error.to_string())
            })?;
        for name in values.keys() {
            super::service::validate_secret_name(name)?;
        }
        Ok(SopsSnapshot {
            revision: Some(revision),
            values,
        })
    }

    /// Returns the encrypted source revision without decrypting it.
    pub(crate) async fn source_revision(
        &self,
    ) -> Result<Option<String>, Report<SecretsServiceError>> {
        let path = self.workspace_directory.join(&self.relative_file);
        validate_secret_file(&path)?;
        read_bounded_file(&path, self.max_document_bytes)
            .await
            .map(|encrypted| encrypted.map(|encrypted| revision(&encrypted)))
    }

    /// Returns the validated relative file identity used to isolate decrypted
    /// caches.
    pub(crate) fn cache_identity(&self) -> String {
        self.relative_file.to_string_lossy().into_owned()
    }

    /// Encrypts and atomically replaces the provider document.
    pub(crate) async fn store(
        &self,
        values: &BTreeMap<String, String>,
    ) -> Result<String, Report<SecretsServiceError>> {
        let plaintext = serde_json::to_vec_pretty(values).map_err(|error| {
            SecretsServiceError::internal("failed to encode the secret document")
                .message(error.to_string())
        })?;
        if u64::try_from(plaintext.len()).map_or(true, |actual| actual > self.max_document_bytes) {
            return Err(SecretsServiceError::invalid_request(format!(
                "secret document exceeds {} bytes",
                self.max_document_bytes
            )));
        }
        let encrypted = self.run(SopsOperation::Encrypt, plaintext).await?;
        ensure_size(
            encrypted.len(),
            self.max_document_bytes,
            "encrypted secret document",
        )?;
        let next_revision = revision(&encrypted);
        let workspace_directory = self.workspace_directory.clone();
        let relative_file = self.relative_file.clone();
        tokio::task::spawn_blocking(move || {
            atomic_write(&workspace_directory, &relative_file, &encrypted)
        })
        .await
        .map_err(|error| {
            SecretsServiceError::internal("secret document write task failed")
                .message(error.to_string())
        })??;
        Ok(next_revision)
    }

    /// Runs one bounded SOPS transform without placing plaintext in arguments
    /// or files.
    #[tracing::instrument(
        level = "debug",
        skip_all,
        fields(operation = operation.name())
    )]
    async fn run(
        &self,
        operation: SopsOperation,
        input: Vec<u8>,
    ) -> Result<Vec<u8>, Report<SecretsServiceError>> {
        let operation_name = operation.name();
        let executable = resolve_host_executable(&self.sops_executable)?;
        let mut command = Command::new(executable);
        command
            .arg(operation.argument())
            .args(["--input-type", "json", "--output-type", "json"])
            .arg("--filename-override")
            .arg(&self.relative_file)
            .current_dir(&self.workspace_directory)
            .env_remove("SOPS_CONFIG")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn().map_err(|error| {
            SecretsServiceError::unavailable(format!("failed to start sops for {operation_name}"))
                .message(error.to_string())
        })?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| SecretsServiceError::internal("failed to open sops standard input"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| SecretsServiceError::internal("failed to open sops standard output"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| SecretsServiceError::internal("failed to open sops standard error"))?;
        let max_output_bytes = self.max_document_bytes;
        let input_task = tokio::spawn(async move {
            stdin.write_all(&input).await?;
            stdin.shutdown().await
        });
        let output_task = tokio::spawn(read_bounded(stdout, max_output_bytes));
        let error_task = tokio::spawn(drain(stderr));

        let status = if let Ok(result) = timeout(self.command_timeout, child.wait()).await {
            result.map_err(|error| {
                SecretsServiceError::unavailable(format!(
                    "failed to wait for sops {operation_name}"
                ))
                .message(error.to_string())
            })?
        } else {
            if let Err(error) = child.kill().await {
                debug!(%error, "failed to terminate timed-out sops process");
            }
            if let Err(error) = child.wait().await {
                debug!(%error, "failed to reap timed-out sops process");
            }
            log_timed_out_task(input_task, "input").await;
            log_timed_out_task(output_task, "output").await;
            log_timed_out_task(error_task, "diagnostic output").await;
            return Err(SecretsServiceError::unavailable(format!(
                "sops {operation_name} timed out"
            )));
        };
        let input_result = input_task.await.map_err(|error| {
            SecretsServiceError::internal("sops input task failed").message(error.to_string())
        })?;
        let output = output_task.await.map_err(|error| {
            SecretsServiceError::internal("sops output task failed").message(error.to_string())
        })??;
        error_task.await.map_err(|error| {
            SecretsServiceError::internal("sops error-output task failed")
                .message(error.to_string())
        })??;
        if !status.success() {
            return Err(SecretsServiceError::unavailable(format!(
                "sops {operation_name} failed"
            )));
        }
        input_result.map_err(|error| {
            SecretsServiceError::unavailable(format!(
                "failed to provide sops {operation_name} input"
            ))
            .message(error.to_string())
        })?;
        Ok(output)
    }
}

/// Decrypted provider state paired with the encrypted source revision.
#[derive(Clone)]
pub(crate) struct SopsSnapshot {
    pub(crate) revision: Option<String>,
    pub(crate) values: BTreeMap<String, String>,
}

impl SopsSnapshot {
    /// Returns the state of a provider whose encrypted file does not exist.
    fn empty() -> Self {
        Self {
            revision: None,
            values: BTreeMap::new(),
        }
    }
}

/// Supported SOPS document transforms.
#[derive(Clone, Copy)]
enum SopsOperation {
    Encrypt,
    Decrypt,
}

impl SopsOperation {
    /// Returns the SOPS command argument for this transform.
    const fn argument(self) -> &'static str {
        match self {
            Self::Encrypt => "encrypt",
            Self::Decrypt => "decrypt",
        }
    }

    /// Returns the operation name used in safe diagnostics.
    const fn name(self) -> &'static str {
        match self {
            Self::Encrypt => "encryption",
            Self::Decrypt => "decryption",
        }
    }
}

/// Reads a bounded regular file without following a final symlink.
async fn read_bounded_file(
    path: &Path,
    max_bytes: u64,
) -> Result<Option<Vec<u8>>, Report<SecretsServiceError>> {
    let metadata = match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(SecretsServiceError::unavailable(
                "failed to inspect encrypted secret document",
            )
            .message(error.to_string()));
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(SecretsServiceError::unavailable(
            "encrypted secret document must be a regular file",
        ));
    }
    if metadata.len() > max_bytes {
        return Err(SecretsServiceError::unavailable(format!(
            "encrypted secret document exceeds {max_bytes} bytes"
        )));
    }
    let bytes = tokio::fs::read(path).await.map_err(|error| {
        SecretsServiceError::unavailable("failed to read encrypted secret document")
            .message(error.to_string())
    })?;
    ensure_size(bytes.len(), max_bytes, "encrypted secret document")?;
    Ok(Some(bytes))
}

/// Drains a stream while retaining no more than the configured byte limit.
async fn read_bounded(
    mut input: impl AsyncRead + Unpin,
    max_bytes: u64,
) -> Result<Vec<u8>, Report<SecretsServiceError>> {
    let mut result = Vec::new();
    let mut buffer = [0_u8; 8192];
    let mut exceeded = false;
    loop {
        let count = input.read(&mut buffer).await.map_err(|error| {
            SecretsServiceError::unavailable("failed to read sops output")
                .message(error.to_string())
        })?;
        if count == 0 {
            break;
        }
        if !exceeded {
            let remaining = usize::try_from(max_bytes)
                .unwrap_or(usize::MAX)
                .saturating_sub(result.len());
            let retained = remaining.min(count);
            result.extend_from_slice(&buffer[..retained]);
            exceeded = retained < count;
        }
    }
    if exceeded {
        return Err(SecretsServiceError::unavailable(format!(
            "sops output exceeds {max_bytes} bytes"
        )));
    }
    Ok(result)
}

/// Drains diagnostic output without retaining potentially sensitive text.
async fn drain(mut input: impl AsyncRead + Unpin) -> Result<(), Report<SecretsServiceError>> {
    let mut buffer = [0_u8; 8192];
    loop {
        let count = input.read(&mut buffer).await.map_err(|error| {
            SecretsServiceError::unavailable("failed to drain sops diagnostic output")
                .message(error.to_string())
        })?;
        if count == 0 {
            return Ok(());
        }
    }
}

/// Logs task failures encountered while reaping a timed-out SOPS command.
async fn log_timed_out_task<T, E>(task: tokio::task::JoinHandle<Result<T, E>>, stream: &'static str)
where
    E: std::fmt::Display,
{
    match task.await {
        Ok(Ok(_)) => {}
        Ok(Err(error)) => debug!(%error, stream, "timed-out sops stream task failed"),
        Err(error) => debug!(%error, stream, "timed-out sops stream task did not join"),
    }
}

/// Validates a provider file as a traversal-free relative JSON path.
fn validate_relative_file(value: &str) -> Result<PathBuf, Report<SecretsServiceError>> {
    let path = Path::new(value);
    if value.is_empty()
        || path.is_absolute()
        || path.extension().is_none_or(|extension| extension != "json")
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(SecretsServiceError::invalid_request(
            "secret provider file must be a relative JSON path without traversal",
        ));
    }
    Ok(path.to_owned())
}

/// Resolves bare executable names only through absolute host PATH entries so a
/// provider working directory can never supply the program being executed.
fn resolve_host_executable(program: &Path) -> Result<PathBuf, Report<SecretsServiceError>> {
    if program.is_absolute() {
        return Ok(program.to_owned());
    }
    let search_path = std::env::var_os("PATH").ok_or_else(|| {
        SecretsServiceError::unavailable(
            "PATH is unset and the SOPS executable is not an absolute path",
        )
    })?;
    std::env::split_paths(&search_path)
        .filter(|directory| directory.is_absolute())
        .map(|directory| directory.join(program))
        .find(|candidate| {
            std::fs::metadata(candidate).is_ok_and(|metadata| {
                metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
            })
        })
        .ok_or_else(|| SecretsServiceError::unavailable("SOPS executable was not found in PATH"))
}

/// Requires the workspace configuration path to be a real directory.
fn validate_workspace_directory(path: &Path) -> Result<(), Report<SecretsServiceError>> {
    let metadata = std::fs::symlink_metadata(path).map_err(|error| {
        SecretsServiceError::unavailable("failed to inspect workspace configuration directory")
            .message(error.to_string())
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(SecretsServiceError::unavailable(
            "workspace configuration path must be a real directory",
        ));
    }
    Ok(())
}

/// Requires every existing provider-file parent to be a real directory.
fn validate_parent_directories(
    workspace_directory: &Path,
    relative_file: &Path,
) -> Result<(), Report<SecretsServiceError>> {
    let mut current = workspace_directory.to_owned();
    if let Some(parent) = relative_file.parent() {
        for component in parent.components() {
            let Component::Normal(component) = component else {
                return Err(SecretsServiceError::invalid_request(
                    "secret provider file has an invalid parent directory",
                ));
            };
            current.push(component);
            let metadata = std::fs::symlink_metadata(&current).map_err(|error| {
                SecretsServiceError::unavailable(
                    "failed to inspect secret provider parent directory",
                )
                .message(error.to_string())
            })?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(SecretsServiceError::unavailable(
                    "secret provider parent path must contain only real directories",
                ));
            }
        }
    }
    Ok(())
}

/// Accepts an absent provider file or an existing regular file.
fn validate_secret_file(path: &Path) -> Result<(), Report<SecretsServiceError>> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => Err(
            SecretsServiceError::unavailable("encrypted secret document must be a regular file"),
        ),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(SecretsServiceError::unavailable(
            "failed to inspect encrypted secret document",
        )
        .message(error.to_string())),
    }
}

/// Enforces a byte limit after fallible conversions.
fn ensure_size(
    actual: usize,
    max_bytes: u64,
    description: &str,
) -> Result<(), Report<SecretsServiceError>> {
    if u64::try_from(actual).map_or(true, |actual| actual > max_bytes) {
        return Err(SecretsServiceError::unavailable(format!(
            "{description} exceeds {max_bytes} bytes"
        )));
    }
    Ok(())
}

/// Computes the public optimistic-concurrency revision of encrypted bytes.
fn revision(encrypted: &[u8]) -> String {
    let digest = Sha256::digest(encrypted);
    let mut revision = String::with_capacity(digest.len() * 2);
    for byte in digest {
        write!(&mut revision, "{byte:02x}").expect("writing to a string cannot fail");
    }
    revision
}

/// Publishes encrypted bytes durably through a mode-`0600` temporary file.
fn atomic_write(
    workspace_directory: &Path,
    relative_file: &Path,
    contents: &[u8],
) -> Result<(), Report<SecretsServiceError>> {
    validate_workspace_directory(workspace_directory)?;
    validate_parent_directories(workspace_directory, relative_file)?;
    let target = workspace_directory.join(relative_file);
    validate_secret_file(&target)?;
    let parent = target.parent().ok_or_else(|| {
        SecretsServiceError::internal("secret provider path has no parent directory")
    })?;
    let temporary = parent.join(format!(".tascarrel-secrets-{}.tmp", Uuid::new_v4()));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
            .map_err(|error| {
                SecretsServiceError::unavailable("failed to create temporary secret document")
                    .message(error.to_string())
            })?;
        file.write_all(contents).map_err(|error| {
            SecretsServiceError::unavailable("failed to write encrypted secret document")
                .message(error.to_string())
        })?;
        file.sync_all().map_err(|error| {
            SecretsServiceError::unavailable("failed to synchronize encrypted secret document")
                .message(error.to_string())
        })?;
        std::fs::rename(&temporary, &target).map_err(|error| {
            SecretsServiceError::unavailable("failed to replace encrypted secret document")
                .message(error.to_string())
        })?;
        std::fs::File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| {
                SecretsServiceError::unavailable("failed to synchronize secret provider directory")
                    .message(error.to_string())
            })?;
        Ok(())
    })();
    if result.is_err()
        && let Err(error) = std::fs::remove_file(&temporary)
    {
        debug!(%error, path = %temporary.display(), "failed to remove temporary secret document");
    }
    result
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;

    use super::*;

    /// Exercises the provider's subprocess working directory, in-memory round
    /// trip, and atomic encrypted file publication through a deterministic
    /// SOPS-compatible test double.
    #[tokio::test]
    async fn provider_round_trip_keeps_plaintext_out_of_the_provider_file() {
        let workspace = tempfile::tempdir().unwrap();
        fs::write(workspace.path().join(".sops.yaml"), "creation_rules: []\n").unwrap();
        let executable = workspace.path().join("fake-sops");
        fs::write(
            &executable,
            "#!/bin/sh\nset -eu\ntest -f .sops.yaml\noperation=$1\nshift\nwhile [ \"$#\" -gt 1 ]; do shift; done\ncase \"$operation\" in\n  encrypt) base64 ;;\n  decrypt) base64 -d ;;\n  *) exit 2 ;;\nesac\n",
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let provider = SopsProvider::new(
            workspace.path().to_owned(),
            None,
            executable,
            Duration::from_secs(5),
            1024 * 1024,
        )
        .unwrap();
        let values = BTreeMap::from([
            ("API_TOKEN".to_owned(), "super-secret".to_owned()),
            ("USERNAME".to_owned(), "tascarrel".to_owned()),
        ]);

        let revision = provider.store(&values).await.unwrap();
        let encrypted = fs::read(workspace.path().join("secrets.json")).unwrap();
        let snapshot = provider.load().await.unwrap();

        assert!(
            !encrypted
                .windows("super-secret".len())
                .any(|value| value == b"super-secret")
        );
        assert_eq!(snapshot.revision.as_deref(), Some(revision.as_str()));
        assert_eq!(snapshot.values, values);
        assert_eq!(
            fs::metadata(workspace.path().join("secrets.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o077,
            0,
        );
    }
}
