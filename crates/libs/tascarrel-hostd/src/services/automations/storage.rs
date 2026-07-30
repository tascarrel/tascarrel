//! Permanent filesystem storage for Automation records and output.

use std::fs;
use std::fs::DirBuilder;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::BufRead as _;
use std::io::BufReader;
use std::io::Write as _;
use std::os::unix::fs::DirBuilderExt as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::path::PathBuf;

use reportify::ErrorExt as _;
use reportify::Report;
use serde::Serialize;
use serde::de::DeserializeOwned;
use thiserror::Error;

use super::StoredExecution;

/// Private durable storage for execution records and output streams.
#[derive(Clone, Debug)]
pub(crate) struct AutomationStorage {
    root: PathBuf,
}

impl AutomationStorage {
    /// Opens or creates a private storage root.
    pub(crate) fn open(root: impl Into<PathBuf>) -> Result<Self, Report<StorageError>> {
        let root = root.into();
        ensure_private_directory(&root)?;
        Ok(Self { root })
    }

    /// Creates the durable directory for one admitted execution.
    pub(crate) fn prepare_execution(&self, id: &str) -> Result<(), Report<StorageError>> {
        let directory = self.root.join(id);
        let mut builder = DirBuilder::new();
        builder.mode(0o700);
        builder
            .create(&directory)
            .map_err(|error| io("create Automation execution directory", error))?;
        sync_directory(&self.root)
    }

    /// Loads every retained execution record.
    pub(crate) fn load(&self) -> Result<Vec<StoredExecution>, Report<StorageError>> {
        let mut records = Vec::new();
        for entry in fs::read_dir(&self.root).map_err(|error| io("list Automation state", error))? {
            let entry = entry.map_err(|error| io("read Automation state entry", error))?;
            let metadata = fs::symlink_metadata(entry.path())
                .map_err(|error| io("inspect Automation state entry", error))?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                continue;
            }
            let record = entry.path().join(RECORD_FILE);
            if record.exists() {
                records.push(read_json(&record)?);
            }
        }
        Ok(records)
    }

    /// Atomically replaces one execution record and synchronizes its directory.
    pub(crate) fn write_record(
        &self,
        execution: &StoredExecution,
    ) -> Result<(), Report<StorageError>> {
        let directory = self.root.join(execution.execution.id.0.as_ref());
        let destination = directory.join(RECORD_FILE);
        let temporary = directory.join(format!(".record-{}.tmp", uuid::Uuid::new_v4()));
        let bytes = serde_json::to_vec_pretty(execution)
            .map_err(|error| encode("encode Automation execution record", error))?;
        if bytes.len() as u64 > MAX_RECORD_BYTES {
            return Err(StorageError::RecordTooLarge.report());
        }
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .custom_flags(nix::libc::O_CLOEXEC | nix::libc::O_NOFOLLOW)
            .open(&temporary)
            .map_err(|error| io("create Automation execution record", error))?;
        file.write_all(&bytes)
            .map_err(|error| io("write Automation execution record", error))?;
        file.sync_all()
            .map_err(|error| io("sync Automation execution record", error))?;
        fs::rename(&temporary, &destination)
            .map_err(|error| io("publish Automation execution record", error))?;
        sync_directory(&directory)
    }

    /// Appends and synchronizes one output record.
    pub(crate) fn append_output<T: Serialize>(
        &self,
        id: &str,
        value: &T,
    ) -> Result<(), Report<StorageError>> {
        append_json_line(&self.root.join(id).join(OUTPUT_FILE), value)
    }

    /// Reads the complete retained output stream for one execution.
    pub(crate) fn read_output<T: DeserializeOwned>(
        &self,
        id: &str,
    ) -> Result<Vec<T>, Report<StorageError>> {
        read_json_lines(&self.root.join(id).join(OUTPUT_FILE))
    }
}

/// Durable storage failures relevant to the Automation service.
#[derive(Debug, Error)]
pub(crate) enum StorageError {
    /// A filesystem operation failed.
    #[error("Automation storage I/O failed")]
    Io,
    /// Retained JSON could not be decoded.
    #[error("Automation storage contains invalid JSON")]
    InvalidJson,
    /// A retained record exceeded its defensive bound.
    #[error("Automation storage record exceeds its size limit")]
    RecordTooLarge,
    /// A storage path could redirect access outside the private root.
    #[error("Automation storage path is unsafe: {0}")]
    UnsafePath(PathBuf),
}

const RECORD_FILE: &str = "record.json";
const OUTPUT_FILE: &str = "output.jsonl";
const MAX_RECORD_BYTES: u64 = 8 * 1024 * 1024;
const MAX_OUTPUT_LINE_BYTES: usize = 1024 * 1024;

fn ensure_private_directory(path: &Path) -> Result<(), Report<StorageError>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(StorageError::UnsafePath(path.to_owned()).report());
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut builder = DirBuilder::new();
            builder.recursive(true).mode(0o700);
            builder
                .create(path)
                .map_err(|error| io("create Automation storage", error))?;
        }
        Err(error) => return Err(io("inspect Automation storage", error)),
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|error| io("secure Automation storage", error))
}

fn append_json_line<T: Serialize>(path: &Path, value: &T) -> Result<(), Report<StorageError>> {
    let mut bytes = serde_json::to_vec(value)
        .map_err(|error| encode("encode Automation output line", error))?;
    if bytes.len() > MAX_OUTPUT_LINE_BYTES {
        return Err(StorageError::RecordTooLarge.report());
    }
    bytes.push(b'\n');
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(nix::libc::O_CLOEXEC | nix::libc::O_NOFOLLOW)
        .open(path)
        .map_err(|error| io("open Automation output stream", error))?;
    file.write_all(&bytes)
        .map_err(|error| io("append Automation output", error))?;
    file.sync_data()
        .map_err(|error| io("sync Automation output", error))
}

fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T, Report<StorageError>> {
    let metadata =
        fs::metadata(path).map_err(|error| io("inspect Automation execution record", error))?;
    if metadata.len() > MAX_RECORD_BYTES {
        return Err(StorageError::RecordTooLarge.report());
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_CLOEXEC | nix::libc::O_NOFOLLOW)
        .open(path)
        .map_err(|error| io("open Automation execution record", error))?;
    serde_json::from_reader(file)
        .map_err(|error| encode("decode Automation execution record", error))
}

fn read_json_lines<T: DeserializeOwned>(path: &Path) -> Result<Vec<T>, Report<StorageError>> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_CLOEXEC | nix::libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(io("open Automation output stream", error)),
    };
    let mut values = Vec::new();
    for line in BufReader::new(file).lines() {
        let line = line.map_err(|error| io("read Automation output", error))?;
        if line.len() > MAX_OUTPUT_LINE_BYTES {
            return Err(StorageError::RecordTooLarge.report());
        }
        values.push(
            serde_json::from_str(&line)
                .map_err(|error| encode("decode Automation output line", error))?,
        );
    }
    Ok(values)
}

fn sync_directory(path: &Path) -> Result<(), Report<StorageError>> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| io("sync Automation directory", error))
}

fn io(action: &'static str, error: std::io::Error) -> Report<StorageError> {
    error.escalate(StorageError::Io).message(action)
}

fn encode(action: &'static str, error: serde_json::Error) -> Report<StorageError> {
    error.escalate(StorageError::InvalidJson).message(action)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::symlink;

    use tempfile::tempdir;

    use super::AutomationStorage;

    /// Output writes cannot be redirected outside durable state with a symlink.
    #[test]
    fn output_stream_rejects_symbolic_links() {
        let temporary = tempdir().unwrap();
        let storage = AutomationStorage::open(temporary.path().join("automations")).unwrap();
        storage
            .prepare_execution("automation_execution_test")
            .unwrap();
        let outside = temporary.path().join("outside");
        fs::write(&outside, b"unchanged").unwrap();
        symlink(
            &outside,
            temporary
                .path()
                .join("automations/automation_execution_test/output.jsonl"),
        )
        .unwrap();

        assert!(
            storage
                .append_output("automation_execution_test", &"output")
                .is_err()
        );
        assert_eq!(fs::read(outside).unwrap(), b"unchanged");
    }
}
