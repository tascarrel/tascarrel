use std::env;
use std::fs::File;
use std::fs::OpenOptions;
use std::fs::{self};
use std::io::IsTerminal;
use std::io::Read;
use std::io::Write;
use std::io::{self};
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use fs2::FileExt;
use sha2::Digest;
use sha2::Sha256;
use tar::Archive;
use xz2::read::XzDecoder;

use crate::doctor::ResolvedDependencies;
use crate::embedded::EmbeddedPayload;
use crate::service;

const SYSTEM_IMAGE_FILE_NAME: &str = "system.erofs";
const MAX_KERNEL_APPEND_BYTES: u64 = 64 * 1024;
const PROGRESS_WIDTH: u64 = 32;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug)]
pub struct InstallPaths {
    pub state: PathBuf,
    pub binary: PathBuf,
}

#[derive(Debug)]
pub struct PreparedPayload {
    pub image: PathBuf,
    pub kernel: PathBuf,
    pub initrd: PathBuf,
    pub kernel_append: String,
    pub ui: PathBuf,
    _lease: File,
}

impl PreparedPayload {
    pub fn guest(&self) -> tascarrel_host::daemon::GuestPayload {
        tascarrel_host::daemon::GuestPayload {
            image: self.image.clone(),
            kernel: self.kernel.clone(),
            initrd: self.initrd.clone(),
            kernel_append: self.kernel_append.clone(),
            ui: self.ui.clone(),
        }
    }
}

impl InstallPaths {
    pub fn discover() -> Result<Self> {
        let home = absolute_environment("HOME").ok_or_else(|| {
            anyhow!("HOME must name an absolute directory for a per-user installation")
        })?;
        let tascarrel_home = tascarrel_host::TascarrelHome::discover()
            .map_err(|error| anyhow!(error.to_string()))?;
        let binary_directory = absolute_environment("TASCARREL_INSTALL_BIN_DIR")
            .unwrap_or_else(|| home.join(".local/bin"));
        Ok(Self {
            state: tascarrel_home.state(),
            binary: binary_directory.join("tascarrel"),
        })
    }

    fn payload(&self, hash: &str) -> PathBuf {
        self.state.join("payloads").join(hash)
    }
}

pub fn install(payload: EmbeddedPayload, dependencies: &ResolvedDependencies) -> Result<PathBuf> {
    validate_embedded_payload(payload)?;
    let paths = InstallPaths::discover()?;
    let current_executable =
        env::current_exe().context("locate the running tascarrel executable")?;
    install_binary(&current_executable, &paths.binary)?;
    service::install(&paths.binary, dependencies)?;
    Ok(paths.binary)
}

/// Extracts and activates the embedded assets for the running host.
pub fn prepare(payload: EmbeddedPayload) -> Result<PreparedPayload> {
    let (paths, payload_root, _lock) = prepare_payload(payload)?;
    let lease = payload_lease(&payload_root)?;
    prune_payloads(&paths, payload.sha256)?;
    prepared_payload(&payload_root, lease)
}

fn prepare_payload(payload: EmbeddedPayload) -> Result<(InstallPaths, PathBuf, File)> {
    validate_embedded_payload(payload)?;
    let paths = InstallPaths::discover()?;
    create_private_directory(&paths.state)?;
    let lock_path = paths.state.join("install.lock");
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(&lock_path)
        .with_context(|| format!("open installation lock {}", lock_path.display()))?;
    lock.lock_exclusive()
        .with_context(|| format!("lock installation at {}", paths.state.display()))?;

    let payload_root = unpack_payload(payload, &paths)?;
    Ok((paths, payload_root, lock))
}

fn prepared_payload(root: &Path, lease: File) -> Result<PreparedPayload> {
    let kernel_append = read_kernel_append(root)?;
    Ok(PreparedPayload {
        image: root.join(SYSTEM_IMAGE_FILE_NAME),
        kernel: root.join("kernel"),
        initrd: root.join("initrd"),
        kernel_append,
        ui: root.join("ui"),
        _lease: lease,
    })
}

fn unpack_payload(payload: EmbeddedPayload, paths: &InstallPaths) -> Result<PathBuf> {
    let target = paths.payload(payload.sha256);
    match fs::symlink_metadata(&target) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            if validate_payload_directory(&target).is_ok() {
                return Ok(target);
            }
            remove_payload_directory(&target, "incomplete payload")?;
        }
        Ok(_) => bail!(
            "installed payload path is not a real directory: {}",
            target.display()
        ),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).with_context(|| format!("inspect {}", target.display())),
    }

    let parent = target
        .parent()
        .ok_or_else(|| anyhow!("installed payload has no parent directory"))?;
    create_private_directory(parent)?;
    let temporary = temporary_directory(parent, "payload")?;
    let mut cleanup = DirectoryCleanup(Some(temporary.clone()));
    let stderr = io::stderr();
    let mut progress = ProgressReader::new(
        payload.compressed,
        stderr.lock(),
        payload.size,
        stderr.is_terminal(),
    );
    progress.render(true);
    let mut archive = Archive::new(XzDecoder::new(progress));
    // Payload archives are build artifacts; their numeric UID/GID fields must
    // never determine ownership in the installing user's private data tree.
    archive.set_preserve_ownerships(false);
    let extracted = archive.unpack(&temporary);
    let progress = archive.into_inner().into_inner();
    let _ = progress.finish(extracted.is_ok());
    extracted.context("extract embedded Tascarrel payload")?;
    validate_payload_ownership(&temporary)?;
    validate_payload_directory(&temporary)?;
    sync_payload(&temporary)?;
    fs::rename(&temporary, &target)
        .with_context(|| format!("activate installed payload {}", target.display()))?;
    cleanup.0 = None;
    sync_directory(parent)?;
    Ok(target)
}

fn prune_payloads(paths: &InstallPaths, current_hash: &str) -> Result<()> {
    let root = paths.state.join("payloads");
    let entries = match fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("read installed payloads {}", root.display()));
        }
    };
    for entry in entries {
        let entry = entry.context("read installed payload directory entry")?;
        if entry.file_name() == std::ffi::OsStr::new(current_hash) {
            continue;
        }
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)
            .with_context(|| format!("inspect obsolete payload {}", path.display()))?;
        if metadata.is_dir() && !metadata.file_type().is_symlink() {
            let Some(_lease) = exclusive_payload_lease(&path)? else {
                continue;
            };
            remove_payload_directory(&path, "obsolete payload")?;
        } else {
            fs::remove_file(&path)
                .with_context(|| format!("remove obsolete payload entry {}", path.display()))?;
        }
    }
    sync_directory(&root)
}

fn payload_lease(root: &Path) -> Result<File> {
    let path = root.join(".lease");
    let lease = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(nix::libc::O_NOFOLLOW)
        .open(&path)
        .with_context(|| format!("open payload lease {}", path.display()))?;
    if !lease.metadata()?.is_file() {
        bail!("payload lease is not a regular file: {}", path.display());
    }
    FileExt::lock_shared(&lease)
        .with_context(|| format!("lock active payload {}", root.display()))?;
    Ok(lease)
}

fn exclusive_payload_lease(root: &Path) -> Result<Option<File>> {
    let path = root.join(".lease");
    let lease = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(nix::libc::O_NOFOLLOW)
        .open(&path)
        .with_context(|| format!("open obsolete payload lease {}", path.display()))?;
    if !lease.metadata()?.is_file() {
        bail!("payload lease is not a regular file: {}", path.display());
    }
    match lease.try_lock_exclusive() {
        Ok(()) => Ok(Some(lease)),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(None),
        Err(error) => {
            Err(error).with_context(|| format!("lock obsolete payload {}", root.display()))
        }
    }
}

fn remove_payload_directory(path: &Path, label: &str) -> Result<()> {
    make_directories_removable(path)
        .with_context(|| format!("prepare {label} for removal {}", path.display()))?;
    fs::remove_dir_all(path).with_context(|| format!("remove {label} {}", path.display()))
}

fn make_directories_removable(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Ok(());
    }
    let mode = metadata.permissions().mode();
    if mode & 0o700 != 0o700 {
        fs::set_permissions(path, fs::Permissions::from_mode(mode | 0o700))?;
    }
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.is_dir() && !metadata.file_type().is_symlink() {
            make_directories_removable(&entry.path())?;
        }
    }
    Ok(())
}

fn validate_payload_directory(root: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(root)
        .with_context(|| format!("inspect installed payload {}", root.display()))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        bail!(
            "installed payload is not a real directory: {}",
            root.display()
        );
    }
    for name in [SYSTEM_IMAGE_FILE_NAME, "kernel", "initrd", "kernel-append"] {
        ensure_regular_file(&root.join(name), "installed payload asset")?;
    }
    ensure_regular_file(&root.join("ui/index.html"), "installed UI entry point")?;
    read_kernel_append(root)?;
    Ok(())
}

fn validate_payload_ownership(root: &Path) -> Result<()> {
    let expected_owner = (
        nix::unistd::Uid::effective().as_raw(),
        nix::unistd::Gid::effective().as_raw(),
    );
    validate_tree_owner(root, expected_owner)
}

fn validate_tree_owner(path: &Path, expected_owner: (u32, u32)) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect extracted payload ownership {}", path.display()))?;
    if metadata.uid() != expected_owner.0 || metadata.gid() != expected_owner.1 {
        bail!(
            "extracted payload entry is owned by UID {} and GID {}, expected UID {} and GID {}: {}",
            metadata.uid(),
            metadata.gid(),
            expected_owner.0,
            expected_owner.1,
            path.display()
        );
    }
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        for entry in fs::read_dir(path)
            .with_context(|| format!("read extracted payload directory {}", path.display()))?
        {
            validate_tree_owner(&entry?.path(), expected_owner)?;
        }
    }
    Ok(())
}

fn read_kernel_append(root: &Path) -> Result<String> {
    let path = root.join("kernel-append");
    let metadata = ensure_regular_file(&path, "kernel command line")?;
    if metadata.len() > MAX_KERNEL_APPEND_BYTES {
        bail!("kernel command line is too large: {}", path.display());
    }
    let value = fs::read_to_string(&path)
        .with_context(|| format!("read kernel command line {}", path.display()))?;
    let value = value.trim();
    if value.is_empty() {
        bail!("kernel command line is empty: {}", path.display());
    }
    Ok(value.to_owned())
}

fn sync_payload(root: &Path) -> Result<()> {
    for name in [SYSTEM_IMAGE_FILE_NAME, "kernel", "initrd", "kernel-append"] {
        File::open(root.join(name))?.sync_all()?;
    }
    File::open(root.join("ui/index.html"))?.sync_all()?;
    sync_directory(root)
}

fn validate_embedded_payload(payload: EmbeddedPayload) -> Result<()> {
    if payload.architecture != env::consts::ARCH {
        bail!(
            "embedded payload architecture {} does not match executable architecture {}",
            payload.architecture,
            env::consts::ARCH
        );
    }
    validate_hash(payload.sha256)?;
    let size = u64::try_from(payload.compressed.len()).unwrap_or(u64::MAX);
    if size != payload.size {
        bail!(
            "embedded payload has size {size}, expected {}",
            payload.size
        );
    }
    let actual = hex_digest(Sha256::digest(payload.compressed));
    if actual != payload.sha256 {
        bail!(
            "embedded payload hash {actual} does not match {}",
            payload.sha256
        );
    }
    Ok(())
}

fn ensure_regular_file(path: &Path, label: &str) -> Result<fs::Metadata> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect {label} {}", path.display()))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        bail!(
            "{label} is not a regular non-symlink file: {}",
            path.display()
        );
    }
    Ok(metadata)
}

fn install_binary(source: &Path, target: &Path) -> Result<()> {
    if source == target {
        return Ok(());
    }
    let parent = target
        .parent()
        .ok_or_else(|| anyhow!("installed binary has no parent directory"))?;
    create_directory(parent, 0o755)?;
    let (mut temporary, temporary_path) = temporary_file(parent, "tascarrel")?;
    let mut source = File::open(source)
        .with_context(|| format!("open running executable {}", source.display()))?;
    io::copy(&mut source, &mut temporary).context("copy tascarrel executable")?;
    temporary
        .set_permissions(fs::Permissions::from_mode(0o755))
        .context("make installed tascarrel executable")?;
    temporary.sync_all().context("sync installed executable")?;
    fs::rename(&temporary_path, target)
        .with_context(|| format!("activate installed executable {}", target.display()))?;
    sync_directory(parent)
}

fn validate_hash(hash: &str) -> Result<()> {
    if hash.len() != 64
        || !hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        bail!("invalid SHA-256 digest `{hash}`");
    }
    Ok(())
}

fn hex_digest(bytes: impl AsRef<[u8]>) -> String {
    use std::fmt::Write as _;
    let mut output = String::with_capacity(bytes.as_ref().len() * 2);
    for byte in bytes.as_ref() {
        write!(&mut output, "{byte:02x}").expect("write to String cannot fail");
    }
    output
}

pub(crate) fn atomic_write(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("atomic output has no parent: {}", path.display()))?;
    create_private_directory(parent)?;
    let (mut temporary, temporary_path) = temporary_file(parent, "write")?;
    temporary.set_permissions(fs::Permissions::from_mode(mode))?;
    temporary.write_all(bytes)?;
    temporary.sync_all()?;
    fs::rename(&temporary_path, path).with_context(|| format!("activate {}", path.display()))?;
    sync_directory(parent)
}

fn temporary_file(parent: &Path, stem: &str) -> Result<(File, PathBuf)> {
    for _ in 0..100 {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!(".{stem}.{}.{}.tmp", std::process::id(), sequence));
        match OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(file) => return Ok((file, path)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error).with_context(|| format!("create {}", path.display())),
        }
    }
    bail!(
        "could not allocate a temporary file in {}",
        parent.display()
    )
}

fn temporary_directory(parent: &Path, stem: &str) -> Result<PathBuf> {
    for _ in 0..100 {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!(".{stem}.{}.{}.tmp", std::process::id(), sequence));
        match fs::create_dir(&path) {
            Ok(()) => {
                fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
                return Ok(path);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error).with_context(|| format!("create {}", path.display())),
        }
    }
    bail!(
        "could not allocate a temporary directory in {}",
        parent.display()
    )
}

struct DirectoryCleanup(Option<PathBuf>);

impl Drop for DirectoryCleanup {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = make_directories_removable(&path);
            let _ = fs::remove_dir_all(path);
        }
    }
}

pub(crate) fn create_directory(path: &Path, mode: u32) -> Result<()> {
    let created = match fs::symlink_metadata(path) {
        Ok(_) => false,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(path)
                .with_context(|| format!("create directory {}", path.display()))?;
            true
        }
        Err(error) => return Err(error).with_context(|| format!("inspect {}", path.display())),
    };
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        bail!("path is not a real directory: {}", path.display());
    }
    if created {
        fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    }
    Ok(())
}

fn create_private_directory(path: &Path) -> Result<()> {
    create_directory(path, 0o700)
}

fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)?
        .sync_all()
        .with_context(|| format!("sync directory {}", path.display()))
}

fn absolute_environment(name: &str) -> Option<PathBuf> {
    env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
}

struct ProgressReader<R, W> {
    inner: R,
    output: W,
    position: u64,
    total: u64,
    last_cells: Option<u64>,
    enabled: bool,
}

impl<R, W> ProgressReader<R, W>
where
    W: Write,
{
    fn new(inner: R, output: W, total: u64, enabled: bool) -> Self {
        Self {
            inner,
            output,
            position: 0,
            total,
            last_cells: None,
            enabled,
        }
    }

    fn render(&mut self, force: bool) {
        if !self.enabled {
            return;
        }
        let cells = progress_ratio(self.position, self.total, PROGRESS_WIDTH);
        if !force && self.last_cells == Some(cells) {
            return;
        }
        self.last_cells = Some(cells);
        let percent = progress_ratio(self.position, self.total, 100);
        let filled = usize::try_from(cells).unwrap_or(usize::MAX);
        let empty = usize::try_from(PROGRESS_WIDTH.saturating_sub(cells)).unwrap_or(0);
        let _ = write!(
            self.output,
            "\rExtracting payload [{}{}] {percent:>3}% {} / {}",
            "#".repeat(filled),
            "-".repeat(empty),
            human_bytes(self.position.min(self.total)),
            human_bytes(self.total),
        );
        let _ = self.output.flush();
    }

    fn finish(mut self, completed: bool) -> (R, W) {
        if completed {
            self.position = self.total;
        }
        self.render(true);
        if self.enabled {
            let _ = writeln!(self.output);
        }
        (self.inner, self.output)
    }
}

impl<R, W> Read for ProgressReader<R, W>
where
    R: Read,
    W: Write,
{
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(buffer)?;
        self.position = self
            .position
            .saturating_add(u64::try_from(read).unwrap_or(u64::MAX));
        self.render(false);
        Ok(read)
    }
}

fn progress_ratio(position: u64, total: u64, scale: u64) -> u64 {
    if total == 0 {
        return scale;
    }
    let scaled = u128::from(position.min(total)) * u128::from(scale) / u128::from(total);
    u64::try_from(scaled).unwrap_or(scale)
}

fn human_bytes(bytes: u64) -> String {
    for (unit, size) in [
        ("GiB", 1024_u64.pow(3)),
        ("MiB", 1024_u64.pow(2)),
        ("KiB", 1024_u64),
    ] {
        if bytes >= size {
            let tenths = bytes.saturating_mul(10) / size;
            return format!("{}.{:01} {unit}", tenths / 10, tenths % 10);
        }
    }
    format!("{bytes} B")
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use tar::Builder;
    use xz2::write::XzEncoder;

    use super::*;

    fn fixture() -> EmbeddedPayload {
        let encoder = XzEncoder::new(Vec::new(), 6);
        let mut archive = Builder::new(encoder);
        let archived_owner = (
            different_id(nix::unistd::Uid::effective().as_raw()),
            different_id(nix::unistd::Gid::effective().as_raw()),
        );
        for (name, bytes) in [
            (SYSTEM_IMAGE_FILE_NAME, b"disk".as_slice()),
            ("kernel", b"kernel"),
            ("initrd", b"initrd"),
            ("kernel-append", b"init=/nix/store/example/init\n"),
            ("ui/index.html", b"future asset"),
        ] {
            let mut header = tar::Header::new_gnu();
            header.set_size(u64::try_from(bytes.len()).unwrap());
            header.set_mode(0o600);
            header.set_uid(u64::from(archived_owner.0));
            header.set_gid(u64::from(archived_owner.1));
            header.set_cksum();
            archive.append_data(&mut header, name, bytes).unwrap();
        }
        let compressed = archive.into_inner().unwrap().finish().unwrap().leak();
        EmbeddedPayload {
            architecture: env::consts::ARCH,
            sha256: Box::leak(hex_digest(Sha256::digest(&compressed)).into_boxed_str()),
            size: u64::try_from(compressed.len()).unwrap(),
            compressed,
        }
    }

    fn different_id(id: u32) -> u32 {
        u32::from(id == 0)
    }

    #[test]
    fn payload_is_content_addressed_extracted_and_idempotent() {
        let directory = tempfile::tempdir().unwrap();
        let paths = InstallPaths {
            state: directory.path().join("state"),
            binary: directory.path().join("bin/tascarrel"),
        };
        let payload = fixture();
        validate_embedded_payload(payload).unwrap();
        let first = unpack_payload(payload, &paths).unwrap();
        let second = unpack_payload(payload, &paths).unwrap();
        assert_eq!(first, second);
        validate_payload_ownership(&first).unwrap();
        assert_eq!(
            fs::read(first.join(SYSTEM_IMAGE_FILE_NAME)).unwrap(),
            b"disk"
        );
        assert_eq!(
            fs::read(first.join("ui/index.html")).unwrap(),
            b"future asset"
        );
    }

    #[test]
    fn compressed_corruption_is_rejected() {
        let mut payload = fixture();
        payload.sha256 = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        assert!(validate_embedded_payload(payload).is_err());
    }

    #[test]
    fn payload_pruning_keeps_only_the_current_payload() {
        let directory = tempfile::tempdir().unwrap();
        let paths = InstallPaths {
            state: directory.path().join("state"),
            binary: directory.path().join("bin/tascarrel"),
        };
        let payloads = paths.state.join("payloads");
        fs::create_dir_all(payloads.join("current")).unwrap();
        fs::create_dir(payloads.join("obsolete")).unwrap();
        fs::create_dir(payloads.join("obsolete/assets")).unwrap();
        fs::write(payloads.join("obsolete/assets/app.js"), b"asset").unwrap();
        fs::set_permissions(
            payloads.join("obsolete/assets"),
            fs::Permissions::from_mode(0o555),
        )
        .unwrap();
        fs::write(payloads.join("incomplete"), b"stale").unwrap();
        let outside = directory.path().join("outside");
        fs::create_dir(&outside).unwrap();
        symlink(&outside, payloads.join("obsolete-link")).unwrap();

        prune_payloads(&paths, "current").unwrap();

        assert!(payloads.join("current").is_dir());
        assert_eq!(fs::read_dir(&payloads).unwrap().count(), 1);
        assert!(outside.is_dir());
    }

    /// Verifies cleanup defers removal of a payload used by another host.
    #[test]
    fn payload_pruning_preserves_leased_generation() {
        let directory = tempfile::tempdir().unwrap();
        let paths = InstallPaths {
            state: directory.path().join("state"),
            binary: directory.path().join("bin/tascarrel"),
        };
        let payloads = paths.state.join("payloads");
        fs::create_dir_all(payloads.join("current")).unwrap();
        fs::create_dir(payloads.join("active")).unwrap();
        let lease = payload_lease(&payloads.join("active")).unwrap();

        prune_payloads(&paths, "current").unwrap();
        assert!(payloads.join("active").is_dir());

        drop(lease);
        prune_payloads(&paths, "current").unwrap();
        assert!(!payloads.join("active").exists());
    }

    #[test]
    fn extraction_progress_reports_completion() {
        let input = b"payload".as_slice();
        let mut progress = ProgressReader::new(input, Vec::new(), 7, true);
        io::copy(&mut progress, &mut io::sink()).unwrap();
        let (_, output) = progress.finish(true);
        let output = String::from_utf8(output).unwrap();
        assert!(output.ends_with("[################################] 100% 7 B / 7 B\n"));
    }

    #[test]
    fn extraction_progress_is_quiet_when_disabled() {
        let input = b"payload".as_slice();
        let mut progress = ProgressReader::new(input, Vec::new(), 7, false);
        io::copy(&mut progress, &mut io::sink()).unwrap();
        let (_, output) = progress.finish(true);
        assert!(output.is_empty());
    }
}
