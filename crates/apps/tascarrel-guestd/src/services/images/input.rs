//! Deterministic image-definition provenance from a host-backed directory.

use std::fs;
use std::fs::File;
use std::io;
use std::io::Write;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::MetadataExt as _;
use std::path::Path;
use std::path::PathBuf;
use std::time::SystemTime;

use jiff::Timestamp;
use reportify::ErrorExt as _;
use reportify::Report;
use sha2::Digest as _;
use sha2::Sha256;
use tar::Builder;
use tar::EntryType;
use tar::Header;
use thiserror::Error;

/// Provenance and stable directory used for one image generation attempt.
#[derive(Clone, Debug)]
pub(crate) struct ImageInputSnapshot {
    /// Canonical directory whose contents were fingerprinted.
    pub(crate) directory: PathBuf,
    /// SHA-256 of the deterministic tar encoding.
    pub(crate) sha256: [u8; 32],
    /// Greatest filesystem modification time observed during traversal.
    pub(crate) modified_at: Timestamp,
}

/// Bounds applied while fingerprinting an image-definition directory.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ImageInputLimits {
    /// Maximum number of archived filesystem entries.
    pub(crate) entries: u64,
    /// Maximum aggregate byte length of regular files.
    pub(crate) bytes: u64,
    /// Maximum directory nesting below the input root.
    pub(crate) depth: usize,
}

/// Creates the deterministic tar hash and observes the greatest input mtime.
///
/// The tar stream contains a root directory entry followed by lexically
/// ordered descendants. Ownership and mtimes are normalized in the tar; the
/// greatest filesystem mtime is returned separately as an inexpensive
/// change-detection hint.
pub(crate) fn snapshot(
    directory: &Path,
    limits: ImageInputLimits,
) -> Result<ImageInputSnapshot, Report<ImageInputError>> {
    let metadata = fs::symlink_metadata(directory)
        .map_err(|source| input_error("inspect image input directory", directory, source))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(invalid_input(
            directory,
            "image input root is not a real directory",
        ));
    }
    let directory = fs::canonicalize(directory)
        .map_err(|source| input_error("canonicalize image input directory", directory, source))?;
    let mut writer = HashWriter::new();
    let mut walker = InputWalker {
        root: &directory,
        device: metadata.dev(),
        limits,
        entries: 0,
        bytes: 0,
        modified_at: metadata.modified().map_err(|source| {
            input_error("read image input modification time", &directory, source)
        })?,
    };
    {
        let mut archive = Builder::new(&mut writer);
        archive.mode(tar::HeaderMode::Deterministic);
        walker.append_directory(&mut archive, Path::new(""), 0)?;
        archive.finish().map_err(|source| {
            input_error("finish deterministic image input tar", &directory, source)
        })?;
    }
    let modified_at = Timestamp::try_from(walker.modified_at).map_err(|source| {
        ImageInputError::Invalid
            .report()
            .message(source.to_string())
    })?;
    Ok(ImageInputSnapshot {
        directory,
        sha256: writer.finalize(),
        modified_at,
    })
}

/// Caller-relevant image input fingerprinting failures.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum ImageInputError {
    /// The image directory cannot be read safely.
    #[error("image input is unavailable")]
    Unavailable,
    /// The image directory violates the snapshot contract.
    #[error("image input is invalid")]
    Invalid,
    /// The image directory exceeds a configured resource limit.
    #[error("image input exceeds its resource limit")]
    Limit,
}

struct InputWalker<'root> {
    root: &'root Path,
    device: u64,
    limits: ImageInputLimits,
    entries: u64,
    bytes: u64,
    modified_at: SystemTime,
}

impl InputWalker<'_> {
    fn append_directory<W: Write>(
        &mut self,
        archive: &mut Builder<W>,
        relative: &Path,
        depth: usize,
    ) -> Result<(), Report<ImageInputError>> {
        self.require_depth(relative, depth)?;
        self.bump_entry(relative)?;
        let path = self.root.join(relative);
        let before = fs::symlink_metadata(&path)
            .map_err(|source| input_error("inspect image input directory", &path, source))?;
        self.require_directory(&path, &before)?;
        self.observe_mtime(&path, &before)?;
        let archive_path = if relative.as_os_str().is_empty() {
            Path::new(".")
        } else {
            relative
        };
        let mut header = header(0, before.mode() & 0o777, EntryType::Directory);
        archive
            .append_data(&mut header, archive_path, io::empty())
            .map_err(|source| input_error("archive image input directory", &path, source))?;

        let mut children = fs::read_dir(&path)
            .map_err(|source| input_error("read image input directory", &path, source))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|source| input_error("read image input directory entry", &path, source))?;
        children.sort_by(|left, right| {
            left.file_name()
                .as_bytes()
                .cmp(right.file_name().as_bytes())
        });
        for child in children {
            let child_relative = relative.join(child.file_name());
            let child_path = child.path();
            let metadata = fs::symlink_metadata(&child_path).map_err(|source| {
                input_error("inspect image input directory entry", &child_path, source)
            })?;
            if metadata.file_type().is_symlink() {
                return Err(invalid_input(
                    &child_path,
                    "image input contains a symbolic link",
                ));
            }
            if metadata.is_dir() {
                self.append_directory(archive, &child_relative, depth + 1)?;
            } else if metadata.is_file() {
                self.append_file(archive, &child_relative, &metadata, depth + 1)?;
            } else {
                return Err(invalid_input(
                    &child_path,
                    "image input contains a special file",
                ));
            }
        }
        let after = fs::symlink_metadata(&path)
            .map_err(|source| input_error("reinspect image input directory", &path, source))?;
        if !same_file(&before, &after) {
            return Err(invalid_input(
                &path,
                "image input directory changed while it was archived",
            ));
        }
        Ok(())
    }

    fn append_file<W: Write>(
        &mut self,
        archive: &mut Builder<W>,
        relative: &Path,
        metadata: &fs::Metadata,
        depth: usize,
    ) -> Result<(), Report<ImageInputError>> {
        self.require_depth(relative, depth)?;
        self.bump_entry(relative)?;
        let path = self.root.join(relative);
        if metadata.dev() != self.device {
            return Err(invalid_input(
                &path,
                "image input crosses a filesystem boundary",
            ));
        }
        self.bytes = self
            .bytes
            .checked_add(metadata.len())
            .filter(|bytes| *bytes <= self.limits.bytes)
            .ok_or_else(|| limit_exceeded(&path, "aggregate file bytes"))?;
        self.observe_mtime(&path, metadata)?;
        let mut file = File::open(&path)
            .map_err(|source| input_error("open image input file", &path, source))?;
        let opened = file
            .metadata()
            .map_err(|source| input_error("inspect opened image input file", &path, source))?;
        if !same_file(metadata, &opened) || opened.len() != metadata.len() {
            return Err(invalid_input(
                &path,
                "image input file changed while it was opened",
            ));
        }
        let mut header = header(opened.len(), opened.mode() & 0o777, EntryType::Regular);
        archive
            .append_data(&mut header, relative, &mut file)
            .map_err(|source| input_error("archive image input file", &path, source))?;
        let after = file
            .metadata()
            .map_err(|source| input_error("reinspect image input file", &path, source))?;
        if !same_file(&opened, &after)
            || opened.len() != after.len()
            || opened.mtime() != after.mtime()
            || opened.mtime_nsec() != after.mtime_nsec()
        {
            return Err(invalid_input(
                &path,
                "image input file changed while it was archived",
            ));
        }
        Ok(())
    }

    fn require_directory(
        &self,
        path: &Path,
        metadata: &fs::Metadata,
    ) -> Result<(), Report<ImageInputError>> {
        if metadata.file_type().is_symlink() || !metadata.is_dir() || metadata.dev() != self.device
        {
            Err(invalid_input(
                path,
                "image input directory is unsafe or crosses a filesystem boundary",
            ))
        } else {
            Ok(())
        }
    }

    fn observe_mtime(
        &mut self,
        path: &Path,
        metadata: &fs::Metadata,
    ) -> Result<(), Report<ImageInputError>> {
        let modified = metadata
            .modified()
            .map_err(|source| input_error("read image input modification time", path, source))?;
        self.modified_at = self.modified_at.max(modified);
        Ok(())
    }

    fn require_depth(&self, path: &Path, depth: usize) -> Result<(), Report<ImageInputError>> {
        if depth > self.limits.depth {
            Err(limit_exceeded(path, "directory depth"))
        } else {
            Ok(())
        }
    }

    fn bump_entry(&mut self, path: &Path) -> Result<(), Report<ImageInputError>> {
        self.entries = self
            .entries
            .checked_add(1)
            .filter(|entries| *entries <= self.limits.entries)
            .ok_or_else(|| limit_exceeded(path, "entry count"))?;
        Ok(())
    }
}

struct HashWriter {
    digest: Sha256,
}

impl HashWriter {
    fn new() -> Self {
        Self {
            digest: Sha256::new(),
        }
    }

    fn finalize(self) -> [u8; 32] {
        self.digest.finalize().into()
    }
}

impl Write for HashWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.digest.update(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn header(size: u64, mode: u32, kind: EntryType) -> Header {
    let mut header = Header::new_gnu();
    header.set_size(size);
    header.set_mode(mode);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header.set_entry_type(kind);
    header.set_cksum();
    header
}

fn same_file(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    left.dev() == right.dev() && left.ino() == right.ino()
}

fn input_error(operation: &'static str, path: &Path, source: io::Error) -> Report<ImageInputError> {
    ImageInputError::Unavailable
        .report()
        .message(operation)
        .field_display("path", path.display())
        .field_display("source", source)
}

fn invalid_input(path: &Path, message: &'static str) -> Report<ImageInputError> {
    ImageInputError::Invalid
        .report()
        .message(message)
        .field_display("path", path.display())
}

fn limit_exceeded(path: &Path, limit: &'static str) -> Report<ImageInputError> {
    ImageInputError::Limit
        .report()
        .field("limit", limit)
        .field_display("path", path.display())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use tempfile::tempdir;

    use super::*;

    /// Verifies enumeration order and filesystem mtimes do not affect the tar
    /// hash.
    #[test]
    fn fingerprint_is_deterministic_and_mtime_is_separate() {
        let temporary = tempdir().expect("temporary directory is created");
        let input = temporary.path().join("image");
        fs::create_dir(&input).expect("input directory is created");
        fs::write(input.join("z"), b"last").expect("fixture is written");
        fs::write(input.join("a"), b"first").expect("fixture is written");
        let first = snapshot(&input, test_limits()).expect("first snapshot succeeds");

        let permissions = fs::Permissions::from_mode(0o640);
        fs::set_permissions(input.join("a"), permissions).expect("fixture mode changes");
        let changed_mode = snapshot(&input, test_limits()).expect("second snapshot succeeds");
        assert_ne!(first.sha256, changed_mode.sha256);
        assert!(changed_mode.modified_at >= first.modified_at);

        let repeated = snapshot(&input, test_limits()).expect("repeated snapshot succeeds");
        assert_eq!(changed_mode.sha256, repeated.sha256);
    }

    /// Verifies unsafe entries and configured limits stop provenance
    /// generation.
    #[test]
    fn fingerprint_rejects_unsafe_or_excessive_inputs() {
        let temporary = tempdir().expect("temporary directory is created");
        let input = temporary.path().join("image");
        fs::create_dir(&input).expect("input directory is created");
        fs::write(input.join("Dockerfile"), b"FROM scratch\n").expect("fixture is written");
        std::os::unix::fs::symlink("Dockerfile", input.join("link"))
            .expect("fixture symlink is created");
        assert!(snapshot(&input, test_limits()).is_err());
        fs::remove_file(input.join("link")).expect("fixture symlink is removed");

        let limits = ImageInputLimits {
            entries: 1,
            ..test_limits()
        };
        assert!(snapshot(&input, limits).is_err());
    }

    fn test_limits() -> ImageInputLimits {
        ImageInputLimits {
            entries: 16,
            bytes: 1024,
            depth: 4,
        }
    }
}
