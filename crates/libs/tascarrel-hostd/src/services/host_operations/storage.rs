//! Permanent filesystem storage for host operations, audit events, and output.

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

use super::StoredOperation;

const RECORD_FILE: &str = "record.json";
const AUDIT_FILE: &str = "audit.jsonl";
const OUTPUT_FILE: &str = "output.jsonl";
const MAX_RECORD_BYTES: u64 = 4 * 1024 * 1024;
const MAX_EVENT_BYTES: usize = 32 * 1024 * 1024;

#[derive(Clone, Debug)]
pub(crate) struct OperationStorage {
    root: PathBuf,
}

impl OperationStorage {
    pub(crate) fn open(root: impl Into<PathBuf>) -> Result<Self, Report<StorageError>> {
        let root = root.into();
        ensure_private_directory(&root)?;
        Ok(Self { root })
    }

    pub(crate) fn operation_dir(&self, id: &str) -> PathBuf {
        self.root.join(id)
    }

    pub(crate) fn input_dir(&self, id: &str, name: &str) -> PathBuf {
        self.operation_dir(id).join("inputs").join(name)
    }

    pub(crate) fn prepare_operation(&self, id: &str) -> Result<(), Report<StorageError>> {
        let directory = self.operation_dir(id);
        let mut builder = DirBuilder::new();
        builder.mode(0o700);
        builder
            .create(&directory)
            .map_err(|error| io("create host operation directory", error))?;
        let mut inputs = DirBuilder::new();
        inputs.mode(0o700);
        inputs
            .create(directory.join("inputs"))
            .map_err(|error| io("create host operation input directory", error))?;
        let mut work = DirBuilder::new();
        work.mode(0o700);
        work.create(directory.join("work"))
            .map_err(|error| io("create host operation work directory", error))?;
        sync_directory(&self.root)?;
        Ok(())
    }

    pub(crate) fn prepare_input(
        &self,
        id: &str,
        name: &str,
    ) -> Result<PathBuf, Report<StorageError>> {
        let directory = self.input_dir(id, name);
        if directory.exists() {
            return Err(StorageError::AlreadyExists.report());
        }
        let mut builder = DirBuilder::new();
        builder.mode(0o700);
        builder
            .create(&directory)
            .map_err(|error| io("create host operation input", error))?;
        sync_directory(
            directory
                .parent()
                .expect("operation input always has a parent"),
        )?;
        Ok(directory)
    }

    pub(crate) fn load(&self) -> Result<Vec<StoredOperation>, Report<StorageError>> {
        let mut records = Vec::new();
        for entry in
            fs::read_dir(&self.root).map_err(|error| io("list host operation directory", error))?
        {
            let entry = entry.map_err(|error| io("read host operation directory entry", error))?;
            let metadata = fs::symlink_metadata(entry.path())
                .map_err(|error| io("inspect host operation directory entry", error))?;
            if !metadata.is_dir() || metadata.file_type().is_symlink() {
                continue;
            }
            let path = entry.path().join(RECORD_FILE);
            if !path.exists() {
                continue;
            }
            records.push(read_json(&path)?);
        }
        Ok(records)
    }

    pub(crate) fn write_record(
        &self,
        operation: &StoredOperation,
    ) -> Result<(), Report<StorageError>> {
        let directory = self.operation_dir(operation.operation.id.0.as_ref());
        let final_path = directory.join(RECORD_FILE);
        let temporary = directory.join(format!(".record-{}.tmp", uuid::Uuid::new_v4()));
        let bytes = serde_json::to_vec_pretty(operation)
            .map_err(|error| encode("encode host operation record", error))?;
        if bytes.len() as u64 > MAX_RECORD_BYTES {
            return Err(StorageError::RecordTooLarge.report());
        }
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&temporary)
            .map_err(|error| io("create host operation record", error))?;
        file.write_all(&bytes)
            .map_err(|error| io("write host operation record", error))?;
        file.sync_all()
            .map_err(|error| io("sync host operation record", error))?;
        fs::rename(&temporary, &final_path)
            .map_err(|error| io("publish host operation record", error))?;
        sync_directory(&directory)
    }

    pub(crate) fn append_audit<T: Serialize>(
        &self,
        id: &str,
        entry: &T,
    ) -> Result<(), Report<StorageError>> {
        append_json_line(&self.operation_dir(id).join(AUDIT_FILE), entry)
    }

    pub(crate) fn append_output<T: Serialize>(
        &self,
        id: &str,
        chunk: &T,
    ) -> Result<(), Report<StorageError>> {
        append_json_line(&self.operation_dir(id).join(OUTPUT_FILE), chunk)
    }

    pub(crate) fn read_audit<T: DeserializeOwned>(
        &self,
        id: &str,
    ) -> Result<Vec<T>, Report<StorageError>> {
        read_json_lines(&self.operation_dir(id).join(AUDIT_FILE))
    }

    pub(crate) fn read_output<T: DeserializeOwned>(
        &self,
        id: &str,
    ) -> Result<Vec<T>, Report<StorageError>> {
        read_json_lines(&self.operation_dir(id).join(OUTPUT_FILE))
    }
}

#[derive(Debug, Error)]
pub(crate) enum StorageError {
    #[error("host operation storage I/O failed")]
    Io,
    #[error("host operation storage contains invalid JSON")]
    InvalidJson,
    #[error("host operation record exceeds its size limit")]
    RecordTooLarge,
    #[error("host operation input already exists")]
    AlreadyExists,
    #[error("host operation storage path is unsafe: {0}")]
    UnsafePath(PathBuf),
}

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
                .map_err(|error| io("create host operation storage", error))?;
        }
        Err(error) => return Err(io("inspect host operation storage", error)),
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|error| io("secure host operation storage", error))
}

fn append_json_line<T: Serialize>(path: &Path, value: &T) -> Result<(), Report<StorageError>> {
    let mut bytes =
        serde_json::to_vec(value).map_err(|error| encode("encode host operation event", error))?;
    if bytes.len() > MAX_EVENT_BYTES {
        return Err(StorageError::RecordTooLarge.report());
    }
    bytes.push(b'\n');
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(nix::libc::O_CLOEXEC | nix::libc::O_NOFOLLOW)
        .open(path)
        .map_err(|error| io("open host operation event stream", error))?;
    file.write_all(&bytes)
        .map_err(|error| io("append host operation event", error))?;
    file.sync_data()
        .map_err(|error| io("sync host operation event", error))
}

fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T, Report<StorageError>> {
    let metadata =
        fs::metadata(path).map_err(|error| io("inspect host operation record", error))?;
    if metadata.len() > MAX_RECORD_BYTES {
        return Err(StorageError::RecordTooLarge.report());
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_CLOEXEC | nix::libc::O_NOFOLLOW)
        .open(path)
        .map_err(|error| io("open host operation record", error))?;
    serde_json::from_reader(file).map_err(|error| encode("decode host operation record", error))
}

fn read_json_lines<T: DeserializeOwned>(path: &Path) -> Result<Vec<T>, Report<StorageError>> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_CLOEXEC | nix::libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(io("open host operation event stream", error)),
    };
    let mut values = Vec::new();
    for line in BufReader::new(file).lines() {
        let line = line.map_err(|error| io("read host operation event", error))?;
        if line.len() > MAX_EVENT_BYTES {
            return Err(StorageError::RecordTooLarge.report());
        }
        values.push(
            serde_json::from_str(&line)
                .map_err(|error| encode("decode host operation event", error))?,
        );
    }
    Ok(values)
}

fn sync_directory(path: &Path) -> Result<(), Report<StorageError>> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| io("sync host operation directory", error))
}

fn io(action: &'static str, error: std::io::Error) -> Report<StorageError> {
    error.escalate(StorageError::Io).message(action)
}

fn encode(action: &'static str, error: serde_json::Error) -> Report<StorageError> {
    error.escalate(StorageError::InvalidJson).message(action)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use tempfile::tempdir;

    use super::*;

    /// Verifies that append-only streams never follow attacker-created links.
    #[test]
    fn event_streams_reject_symbolic_links() {
        let temporary = tempdir().unwrap();
        let storage = OperationStorage::open(temporary.path().join("operations")).unwrap();
        storage.prepare_operation("host_operation_test").unwrap();
        let outside = temporary.path().join("outside");
        fs::write(&outside, b"unchanged").unwrap();
        symlink(
            &outside,
            storage
                .operation_dir("host_operation_test")
                .join(OUTPUT_FILE),
        )
        .unwrap();

        assert!(
            storage
                .append_output("host_operation_test", &"output")
                .is_err()
        );
        assert_eq!(fs::read(&outside).unwrap(), b"unchanged");
    }
}
