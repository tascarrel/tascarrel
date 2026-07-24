//! Safe, deterministic snapshots of host workspace inputs.
//!
//! The host encodes conventional workspace inputs into a bounded archive. The
//! guest validates and atomically publishes that archive as an immutable input
//! generation.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::collections::BTreeSet;
use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::io::Read;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

use sha2::Digest;
use sha2::Sha256;
use tar::Archive;
use tar::Builder;
use tar::EntryType;
use tar::Header;
use thiserror::Error;

/// Maximum encoded workspace snapshot accepted on either side.
pub const MAX_ARCHIVE_BYTES: u64 = 512 * 1024 * 1024;
/// Maximum number of filesystem entries in one snapshot.
const MAX_ENTRIES: usize = 100_000;

/// Workspace snapshot construction or publication error.
#[derive(Debug, Error)]
pub enum TransferError {
    /// Filesystem or archive I/O failed.
    #[error("workspace snapshot I/O failed: {0}")]
    Io(#[from] io::Error),
    /// An input or archive entry violated the snapshot contract.
    #[error("unsafe workspace snapshot: {0}")]
    Unsafe(String),
    /// The bounded snapshot limit was exceeded.
    #[error("workspace snapshot exceeds its resource limit: {0}")]
    Limit(String),
}

/// Creates a deterministic tar snapshot containing `config.toml`, `image/`,
/// and the conventional optional `.env`, `overlay/`, `hooks/{setup,init}/`,
/// and `agents/{AGENTS.md,skills/}` inputs.
///
/// The destination is replaced atomically and never follows input symlinks.
///
/// # Errors
///
/// Returns an error for unsafe input types, racing inputs, resource-limit
/// violations, or filesystem failures.
pub fn create_snapshot(root: &Path, destination: &Path) -> Result<(), TransferError> {
    require_real_directory(root, "workspace root")?;
    let config_path = root.join("config.toml");
    let config = match read_regular_bounded(&config_path, 64 * 1024) {
        Ok(bytes) => bytes,
        Err(TransferError::Io(error)) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(error),
    };
    let parent = destination
        .parent()
        .ok_or_else(|| TransferError::Unsafe("snapshot has no parent".to_owned()))?;
    require_real_directory(parent, "snapshot parent")?;
    let temporary = tempfile::Builder::new()
        .prefix(".workspace-input-")
        .tempfile_in(parent)?;
    let (mut output, temporary_path) = temporary.keep().map_err(|error| error.error)?;
    let result = (|| {
        let mut builder = Builder::new(&mut output);
        builder.mode(tar::HeaderMode::Deterministic);
        let mut count = 0_usize;
        append_bytes(
            &mut builder,
            Path::new("config.toml"),
            &config,
            0o644,
            &mut count,
        )?;
        let environment = match read_regular_bounded(&root.join(".env"), 64 * 1024) {
            Ok(bytes) => bytes,
            Err(TransferError::Io(error)) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(error),
        };
        append_bytes(
            &mut builder,
            Path::new(".env"),
            &environment,
            0o600,
            &mut count,
        )?;
        append_tree(&mut builder, root, Path::new("image"), &mut count)?;
        match fs::symlink_metadata(root.join("overlay")) {
            Ok(_) => append_tree(&mut builder, root, Path::new("overlay"), &mut count)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                append_empty_directory(&mut builder, Path::new("overlay"), &mut count)?;
            }
            Err(error) => return Err(error.into()),
        }
        append_hooks(&mut builder, root, &mut count)?;
        append_agents(&mut builder, root, &mut count)?;
        builder.finish()?;
        drop(builder);
        output.sync_all()?;
        inspect_snapshot(&temporary_path)?;
        fs::rename(&temporary_path, destination)?;
        sync_directory(parent)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary_path);
    }
    result
}

/// Validates and publishes a received snapshot beneath `root/current`.
///
/// Publication uses an atomically replaced relative symlink, so readers see
/// either the previous complete generation or the new complete generation.
///
/// # Errors
///
/// Returns an error for malformed archives, unsafe paths or types, excessive
/// resources, and filesystem failures.
pub fn publish_snapshot(archive: &Path, root: &Path) -> Result<PathBuf, TransferError> {
    fs::create_dir_all(root)?;
    require_real_directory(root, "workspace input root")?;
    let snapshot_digest = inspect_snapshot(archive)?;
    let digest = snapshot_digest.iter().fold(
        String::with_capacity(snapshot_digest.len() * 2),
        |mut output, byte| {
            use std::fmt::Write as _;
            write!(output, "{byte:02x}").expect("writing to a String cannot fail");
            output
        },
    );
    let generation_name = format!("generation-{digest}");
    let generation = root.join(&generation_name);
    prepare_generation(archive, root, &generation)?;
    validate_generation(&generation)?;
    publish_generation(root, &generation_name)?;
    Ok(generation)
}

/// Extracts a snapshot into a private staging directory before exposing its
/// content-addressed generation.
fn prepare_generation(archive: &Path, root: &Path, generation: &Path) -> Result<(), TransferError> {
    match fs::symlink_metadata(generation) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            if validate_generation(generation).is_ok() {
                return Ok(());
            }
        }
        Ok(_) => {
            return Err(TransferError::Unsafe(format!(
                "snapshot generation is not a real directory: {}",
                generation.display()
            )));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }

    let staging = root.join(format!(".generation-{}", uuid_like()));
    fs::create_dir(&staging)?;
    let result = extract_snapshot(archive, &staging)
        .and_then(|()| validate_generation(&staging))
        .and_then(|()| {
            match fs::symlink_metadata(generation) {
                Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
                    fs::remove_dir_all(generation)?;
                }
                Ok(_) => {
                    return Err(TransferError::Unsafe(format!(
                        "snapshot generation is not a real directory: {}",
                        generation.display()
                    )));
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            fs::rename(&staging, generation)?;
            sync_directory(root)
        });
    if result.is_err()
        && let Err(error) = fs::remove_dir_all(&staging)
        && error.kind() != io::ErrorKind::NotFound
    {
        return Err(error.into());
    }
    result
}

/// Validates the complete generation contract before publication.
fn validate_generation(generation: &Path) -> Result<(), TransferError> {
    for directory in [
        "image",
        "overlay",
        "hooks",
        "hooks/setup",
        "hooks/init",
        "agents",
        "agents/skills",
    ] {
        require_real_directory(
            &generation.join(directory),
            "snapshot conventional directory",
        )?;
    }
    let config = generation.join("config.toml");
    if !config.is_file() {
        return Err(TransferError::Unsafe(
            "snapshot lacks config.toml".to_owned(),
        ));
    }
    let environment = generation.join(".env");
    if !environment.is_file() {
        return Err(TransferError::Unsafe("snapshot lacks .env".to_owned()));
    }
    Ok(())
}

/// Atomically changes the current-generation link after validation succeeds.
fn publish_generation(root: &Path, generation_name: &str) -> Result<(), TransferError> {
    let link = root.join(format!(".current-{}", uuid_like()));
    std::os::unix::fs::symlink(generation_name, &link)?;
    fs::rename(&link, root.join("current"))?;
    sync_directory(root)?;
    Ok(())
}

fn append_hooks<W: Write>(
    builder: &mut Builder<W>,
    root: &Path,
    count: &mut usize,
) -> Result<(), TransferError> {
    let hooks = root.join("hooks");
    match fs::symlink_metadata(&hooks) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            bump(count)?;
            let mut header = make_header(
                0,
                metadata.permissions().mode() & 0o777,
                EntryType::Directory,
            );
            builder.append_data(&mut header, Path::new("hooks"), io::empty())?;
            for entry in fs::read_dir(&hooks)? {
                let entry = entry?;
                if entry.file_name() != "setup" && entry.file_name() != "init" {
                    return Err(TransferError::Unsafe(format!(
                        "hooks contains unsupported entry {}",
                        entry.path().display()
                    )));
                }
            }
            for name in ["setup", "init"] {
                let relative = Path::new("hooks").join(name);
                match fs::symlink_metadata(root.join(&relative)) {
                    Ok(_) => append_tree(builder, root, &relative, count)?,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        append_empty_directory(builder, &relative, count)?;
                    }
                    Err(error) => return Err(error.into()),
                }
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            append_empty_directory(builder, Path::new("hooks"), count)?;
            append_empty_directory(builder, Path::new("hooks/setup"), count)?;
            append_empty_directory(builder, Path::new("hooks/init"), count)?;
        }
        Ok(_) => {
            return Err(TransferError::Unsafe(
                "workspace hooks is not a real directory".to_owned(),
            ));
        }
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn append_agents<W: Write>(
    builder: &mut Builder<W>,
    root: &Path,
    count: &mut usize,
) -> Result<(), TransferError> {
    let agents = root.join("agents");
    match fs::symlink_metadata(&agents) {
        Ok(_) => {
            append_tree(builder, root, Path::new("agents"), count)?;
            match fs::symlink_metadata(agents.join("skills")) {
                Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
                Ok(_) => {
                    return Err(TransferError::Unsafe(
                        "workspace agent skills is not a real directory".to_owned(),
                    ));
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    append_empty_directory(builder, Path::new("agents/skills"), count)?;
                }
                Err(error) => return Err(error.into()),
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            append_empty_directory(builder, Path::new("agents"), count)?;
            append_empty_directory(builder, Path::new("agents/skills"), count)?;
        }
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

/// Computes the SHA-256 of a bounded snapshot file.
fn inspect_snapshot(path: &Path) -> Result<[u8; 32], TransferError> {
    let mut file = open_regular(path)?;
    let length = file.metadata()?.len();
    if length > MAX_ARCHIVE_BYTES {
        return Err(TransferError::Limit(format!("archive is {length} bytes")));
    }
    let mut digest = Sha256::new();
    io::copy(&mut file, &mut digest)?;
    Ok(digest.finalize().into())
}

fn append_tree<W: Write>(
    builder: &mut Builder<W>,
    root: &Path,
    relative: &Path,
    count: &mut usize,
) -> Result<(), TransferError> {
    let path = root.join(relative);
    let before = require_real_directory(&path, "workspace input directory")?;
    bump(count)?;
    let mut archive_header =
        make_header(0, before.permissions().mode() & 0o777, EntryType::Directory);
    builder.append_data(&mut archive_header, relative, io::empty())?;
    let mut children = fs::read_dir(&path)?.collect::<Result<Vec<_>, _>>()?;
    children.sort_by(|left, right| {
        left.file_name()
            .as_bytes()
            .cmp(right.file_name().as_bytes())
    });
    for child in children {
        let child_relative = relative.join(child.file_name());
        let metadata = fs::symlink_metadata(child.path())?;
        if metadata.file_type().is_symlink() {
            return Err(TransferError::Unsafe(format!(
                "input contains symlink {}",
                child_relative.display()
            )));
        }
        if metadata.is_dir() {
            append_tree(builder, root, &child_relative, count)?;
        } else if metadata.is_file() {
            bump(count)?;
            let mut file = open_regular(&child.path())?;
            let opened = file.metadata()?;
            if opened.dev() != metadata.dev() || opened.ino() != metadata.ino() {
                return Err(TransferError::Unsafe(format!(
                    "input changed while opening {}",
                    child_relative.display()
                )));
            }
            let mut archive_header = make_header(
                opened.len(),
                opened.permissions().mode() & 0o777,
                EntryType::Regular,
            );
            builder.append_data(&mut archive_header, &child_relative, &mut file)?;
            let after = file.metadata()?;
            if after.len() != opened.len() || after.ino() != opened.ino() {
                return Err(TransferError::Unsafe(format!(
                    "input changed while reading {}",
                    child_relative.display()
                )));
            }
        } else {
            return Err(TransferError::Unsafe(format!(
                "input contains special file {}",
                child_relative.display()
            )));
        }
    }
    let after = fs::symlink_metadata(&path)?;
    if after.dev() != before.dev() || after.ino() != before.ino() {
        return Err(TransferError::Unsafe(format!(
            "input directory changed while reading {}",
            relative.display()
        )));
    }
    Ok(())
}

fn append_bytes<W: Write>(
    builder: &mut Builder<W>,
    path: &Path,
    bytes: &[u8],
    mode: u32,
    count: &mut usize,
) -> Result<(), TransferError> {
    bump(count)?;
    let mut header = make_header(bytes.len() as u64, mode, EntryType::Regular);
    builder.append_data(&mut header, path, bytes)?;
    Ok(())
}

fn append_empty_directory<W: Write>(
    builder: &mut Builder<W>,
    path: &Path,
    count: &mut usize,
) -> Result<(), TransferError> {
    bump(count)?;
    let mut header = make_header(0, 0o755, EntryType::Directory);
    builder.append_data(&mut header, path, io::empty())?;
    Ok(())
}

fn make_header(size: u64, mode: u32, kind: EntryType) -> Header {
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

fn extract_snapshot(archive: &Path, destination: &Path) -> Result<(), TransferError> {
    let file = open_regular(archive)?;
    if file.metadata()?.len() > MAX_ARCHIVE_BYTES {
        return Err(TransferError::Limit(
            "received archive is too large".to_owned(),
        ));
    }
    let mut archive = Archive::new(file);
    let mut seen = BTreeSet::new();
    let mut count = 0_usize;
    let mut total = 0_u64;
    for entry in archive.entries()? {
        let mut entry = entry?;
        bump(&mut count)?;
        let path = entry.path()?.into_owned();
        validate_archive_path(&path)?;
        if !seen.insert(path.clone()) {
            return Err(TransferError::Unsafe(format!(
                "duplicate entry {}",
                path.display()
            )));
        }
        let kind = entry.header().entry_type();
        let target = destination.join(&path);
        if kind.is_dir() {
            create_real_directories(destination, &target)?;
            continue;
        }
        if !kind.is_file() {
            return Err(TransferError::Unsafe(format!(
                "unsupported archive entry {}",
                path.display()
            )));
        }
        let size = entry.size();
        total = total
            .checked_add(size)
            .ok_or_else(|| TransferError::Limit("expanded size overflow".to_owned()))?;
        if total > MAX_ARCHIVE_BYTES {
            return Err(TransferError::Limit(
                "expanded snapshot is too large".to_owned(),
            ));
        }
        let parent = target
            .parent()
            .ok_or_else(|| TransferError::Unsafe("entry has no parent".to_owned()))?;
        create_real_directories(destination, parent)?;
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(entry.header().mode()?.min(0o755))
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(&target)?;
        let copied = io::copy(&mut entry, &mut output)?;
        if copied != size {
            return Err(TransferError::Unsafe(format!(
                "truncated entry {}",
                path.display()
            )));
        }
        output.sync_all()?;
    }
    Ok(())
}

fn create_real_directories(root: &Path, path: &Path) -> Result<(), TransferError> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| TransferError::Unsafe("entry escaped destination".to_owned()))?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(component) = component else {
            return Err(TransferError::Unsafe(
                "invalid destination component".to_owned(),
            ));
        };
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Ok(_) => {
                return Err(TransferError::Unsafe(format!(
                    "non-directory ancestor {}",
                    current.display()
                )));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => fs::create_dir(&current)?,
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn validate_archive_path(path: &Path) -> Result<(), TransferError> {
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(TransferError::Unsafe(format!(
            "invalid archive path {}",
            path.display()
        )));
    }
    Ok(())
}

fn require_real_directory(path: &Path, kind: &str) -> Result<fs::Metadata, TransferError> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(TransferError::Unsafe(format!(
            "{kind} is not a real directory: {}",
            path.display()
        )));
    }
    Ok(metadata)
}

fn read_regular_bounded(path: &Path, maximum: u64) -> Result<Vec<u8>, TransferError> {
    let mut file = open_regular(path)?;
    let length = file.metadata()?.len();
    if length > maximum {
        return Err(TransferError::Limit(format!(
            "{} is too large",
            path.display()
        )));
    }
    let capacity = usize::try_from(length)
        .map_err(|_| TransferError::Limit(format!("{} is too large", path.display())))?;
    let mut bytes = Vec::with_capacity(capacity);
    Read::by_ref(&mut file)
        .take(maximum + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > maximum {
        return Err(TransferError::Limit(format!(
            "{} grew too large",
            path.display()
        )));
    }
    Ok(bytes)
}

fn open_regular(path: &Path) -> Result<File, TransferError> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(TransferError::Unsafe(format!(
            "not a regular file: {}",
            path.display()
        )));
    }
    Ok(file)
}

fn bump(count: &mut usize) -> Result<(), TransferError> {
    *count += 1;
    if *count > MAX_ENTRIES {
        return Err(TransferError::Limit(format!(
            "more than {MAX_ENTRIES} entries"
        )));
    }
    Ok(())
}

fn sync_directory(path: &Path) -> Result<(), TransferError> {
    File::open(path)?.sync_all()?;
    Ok(())
}

fn uuid_like() -> String {
    use std::time::SystemTime;
    use std::time::UNIX_EPOCH;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    format!("{}-{nanos}", std::process::id())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verifies all conventional workspace inputs publish as one generation.
    #[test]
    fn snapshot_publishes_all_conventional_inputs_as_one_generation() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source");
        let destination = directory.path().join("destination");
        fs::create_dir(&source).unwrap();
        fs::create_dir(source.join("image")).unwrap();
        fs::create_dir(source.join("overlay")).unwrap();
        fs::create_dir(source.join("hooks")).unwrap();
        fs::create_dir(source.join("hooks/setup")).unwrap();
        fs::create_dir(source.join("hooks/init")).unwrap();
        fs::create_dir(source.join("agents")).unwrap();
        fs::create_dir(source.join("agents/skills")).unwrap();
        fs::create_dir(source.join("agents/skills/release")).unwrap();
        fs::write(source.join("config.toml"), b"").unwrap();
        fs::write(source.join(".env"), b"TOKEN=one\n").unwrap();
        fs::write(source.join("image/Dockerfile"), b"FROM scratch\n").unwrap();
        fs::write(source.join("overlay/tool"), b"v1\n").unwrap();
        fs::write(source.join("hooks/setup/10-packages"), b"setup\n").unwrap();
        fs::write(source.join("hooks/init/20-agent"), b"init\n").unwrap();
        fs::write(source.join("agents/AGENTS.md"), b"Always test.\n").unwrap();
        fs::write(
            source.join("agents/skills/release/SKILL.md"),
            b"---\nname: release\ndescription: Release safely.\n---\n",
        )
        .unwrap();
        let archive = directory.path().join("snapshot.tar");
        create_snapshot(&source, &archive).unwrap();
        assert!(archive.metadata().unwrap().len() > 0);
        publish_snapshot(&archive, &destination).unwrap();
        assert_eq!(
            fs::read(destination.join("current/image/Dockerfile")).unwrap(),
            b"FROM scratch\n"
        );
        assert_eq!(
            fs::read(destination.join("current/overlay/tool")).unwrap(),
            b"v1\n"
        );
        assert_eq!(
            fs::read(destination.join("current/hooks/setup/10-packages")).unwrap(),
            b"setup\n"
        );
        assert_eq!(
            fs::read(destination.join("current/hooks/init/20-agent")).unwrap(),
            b"init\n"
        );
        assert_eq!(
            fs::read(destination.join("current/agents/AGENTS.md")).unwrap(),
            b"Always test.\n"
        );
        assert!(
            destination
                .join("current/agents/skills/release/SKILL.md")
                .is_file()
        );
        assert_eq!(
            fs::read(destination.join("current/.env")).unwrap(),
            b"TOKEN=one\n"
        );

        fs::write(source.join("overlay/tool"), b"v2\n").unwrap();
        create_snapshot(&source, &archive).unwrap();
        publish_snapshot(&archive, &destination).unwrap();
        assert_eq!(
            fs::read(destination.join("current/overlay/tool")).unwrap(),
            b"v2\n"
        );
        assert_eq!(
            fs::read_dir(&destination)
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| entry.file_name().as_bytes().starts_with(b"generation-"))
                .count(),
            2
        );
    }

    /// Verifies unsafe links are rejected and absent optional inputs are empty.
    #[test]
    fn snapshot_rejects_symlinks_and_synthesizes_empty_optional_directories() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source");
        fs::create_dir(&source).unwrap();
        fs::create_dir(source.join("image")).unwrap();
        fs::write(source.join("config.toml"), b"").unwrap();
        std::os::unix::fs::symlink("config.toml", source.join("image/Dockerfile")).unwrap();
        assert!(create_snapshot(&source, &directory.path().join("one.tar")).is_err());

        fs::remove_file(source.join("image/Dockerfile")).unwrap();
        fs::write(source.join("image/Dockerfile"), b"FROM scratch\n").unwrap();
        let archive = directory.path().join("two.tar");
        create_snapshot(&source, &archive).unwrap();
        let destination = directory.path().join("destination");
        publish_snapshot(&archive, &destination).unwrap();
        assert!(destination.join("current/overlay").is_dir());
        assert!(destination.join("current/hooks/setup").is_dir());
        assert!(destination.join("current/hooks/init").is_dir());
        assert!(destination.join("current/agents/skills").is_dir());
        assert_eq!(fs::read(destination.join("current/.env")).unwrap(), b"");

        fs::create_dir(source.join("agents")).unwrap();
        fs::write(source.join("agents/skills"), b"not a directory").unwrap();
        assert!(create_snapshot(&source, &directory.path().join("three.tar")).is_err());
    }

    /// Publication replaces an incomplete generation left by an interrupted
    /// extraction instead of treating it as a durable cache entry.
    #[test]
    fn snapshot_recovers_from_an_incomplete_generation() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source");
        fs::create_dir(&source).unwrap();
        fs::create_dir(source.join("image")).unwrap();
        fs::write(source.join("config.toml"), b"[env]\nTOKEN = \"value\"\n").unwrap();
        let archive = directory.path().join("snapshot.tar");
        create_snapshot(&source, &archive).unwrap();
        let snapshot_digest = inspect_snapshot(&archive).unwrap();
        let digest = snapshot_digest.iter().fold(
            String::with_capacity(snapshot_digest.len() * 2),
            |mut output, byte| {
                use std::fmt::Write as _;
                write!(output, "{byte:02x}").unwrap();
                output
            },
        );
        let destination = directory.path().join("destination");
        let incomplete = destination.join(format!("generation-{digest}"));
        fs::create_dir_all(incomplete.join("image")).unwrap();

        publish_snapshot(&archive, &destination).unwrap();

        assert_eq!(
            fs::read(destination.join("current/config.toml")).unwrap(),
            b"[env]\nTOKEN = \"value\"\n"
        );
        assert!(destination.join("current/agents/skills").is_dir());
    }
}
