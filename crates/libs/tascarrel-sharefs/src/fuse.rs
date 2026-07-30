//! Kernel FUSE adapter for a copy-on-write share.
//!
//! [`MountedShareFileSystem`] owns a background kernel session and exposes the
//! underlying [`ShareFileSystem`] for change inspection and synchronization.
//! The adapter uses direct I/O and zero cache lifetimes so untouched names
//! continue to reflect the live lower directory.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::os::unix::fs::MetadataExt as _;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use fuser::Errno;
use fuser::FileAttr;
use fuser::FileHandle;
use fuser::FileType;
use fuser::Filesystem;
use fuser::FopenFlags;
use fuser::Generation;
use fuser::INodeNo;
use fuser::KernelConfig;
use fuser::MountOption;
use fuser::OpenFlags;
use fuser::RenameFlags;
use fuser::ReplyAttr;
use fuser::ReplyCreate;
use fuser::ReplyData;
use fuser::ReplyDirectory;
use fuser::ReplyEmpty;
use fuser::ReplyEntry;
use fuser::ReplyOpen;
use fuser::ReplyWrite;
use fuser::Request;
use fuser::SessionACL;
use fuser::TimeOrNow;
use reportify::Report;
use tracing::warn;

use crate::DirectoryEntry;
use crate::EntryKind;
use crate::EntryMetadata;
use crate::ShareFileSystem;
use crate::ShareFsError;
use crate::ShareFsResult;

const ATTRIBUTE_TTL: Duration = Duration::ZERO;
const BLOCK_SIZE: u32 = 4096;
const FUSE_CONNECTIONS_DIRECTORY: &str = "/sys/fs/fuse/connections";
const ROOT_INODE: u64 = 1;

/// Presentation settings for one mounted share.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ShareFileSystemMountOptions {
    /// UID reported for every merged entry.
    pub uid: u32,
    /// GID reported for every merged entry.
    pub gid: u32,
    /// Whether callers other than the mounting UID may access the filesystem.
    pub allow_other: bool,
}

/// One mounted background FUSE session.
pub struct MountedShareFileSystem {
    filesystem: Arc<ShareFileSystem>,
    session: Option<fuser::BackgroundSession>,
    mountpoint: PathBuf,
    connection_device: u64,
    connection_abort: PathBuf,
}

impl MountedShareFileSystem {
    /// Opens a copy-on-write share and mounts it at an existing directory.
    ///
    /// # Errors
    ///
    /// Returns an error when the `ShareFS` state cannot be opened, the
    /// mountpoint is invalid, or the kernel rejects the FUSE session.
    #[tracing::instrument(
        name = "tascarrel_sharefs.mount",
        level = "info",
        skip_all,
        fields(
            lower = %lower.as_ref().display(),
            state = %state.as_ref().display(),
            mountpoint = %mountpoint.as_ref().display()
        ),
        err
    )]
    pub fn mount(
        lower: impl AsRef<Path>,
        state: impl AsRef<Path>,
        mountpoint: impl AsRef<Path>,
        options: ShareFileSystemMountOptions,
    ) -> ShareFsResult<Self> {
        if !Path::new(FUSE_CONNECTIONS_DIRECTORY).is_dir() {
            return Err(Report::new(ShareFsError::Fuse {
                action: "locate the FUSE control filesystem",
                source: std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("{FUSE_CONNECTIONS_DIRECTORY} is not mounted"),
                ),
            }));
        }
        let mountpoint = std::fs::canonicalize(mountpoint.as_ref()).map_err(|source| {
            Report::new(ShareFsError::Fuse {
                action: "resolve the ShareFS mountpoint",
                source,
            })
        })?;
        if !mountpoint.is_dir() {
            return Err(Report::new(ShareFsError::Fuse {
                action: "validate the ShareFS mountpoint",
                source: std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "mountpoint is not a directory",
                ),
            }));
        }
        let filesystem = Arc::new(ShareFileSystem::open(lower, state)?);
        let adapter = FuseAdapter::new(Arc::clone(&filesystem), options.uid, options.gid);
        let mut config = fuser::Config::default();
        config.mount_options = vec![
            MountOption::FSName("tascarrel-sharefs".to_owned()),
            MountOption::Subtype("tascarrel-sharefs".to_owned()),
            MountOption::DefaultPermissions,
            MountOption::NoDev,
            MountOption::NoSuid,
            MountOption::RW,
        ];
        config.acl = if options.allow_other {
            SessionACL::All
        } else {
            SessionACL::Owner
        };
        // ShareFS serializes core transitions, so additional FUSE workers
        // provide no useful parallelism and can delay session shutdown.
        config.n_threads = Some(1);
        config.clone_fd = false;
        let session = fuser::spawn_mount(adapter, &mountpoint, &config).map_err(|source| {
            Report::new(ShareFsError::Fuse {
                action: "mount ShareFS",
                source,
            })
        })?;
        let connection_device = std::fs::metadata(&mountpoint)
            .map_err(|source| {
                Report::new(ShareFsError::Fuse {
                    action: "inspect the ShareFS FUSE connection",
                    source,
                })
            })?
            .dev();
        let connection_abort = Path::new(FUSE_CONNECTIONS_DIRECTORY)
            .join(rustix::fs::minor(connection_device).to_string())
            .join("abort");
        if !connection_abort.is_file() {
            return Err(Report::new(ShareFsError::Fuse {
                action: "locate the ShareFS FUSE connection",
                source: std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("{} is unavailable", connection_abort.display()),
                ),
            }));
        }
        Ok(Self {
            filesystem,
            session: Some(session),
            mountpoint,
            connection_device,
            connection_abort,
        })
    }

    /// Returns the mounted copy-on-write filesystem.
    #[must_use]
    pub fn filesystem(&self) -> &Arc<ShareFileSystem> {
        &self.filesystem
    }

    /// Returns the canonical mountpoint.
    #[must_use]
    pub fn mountpoint(&self) -> &Path {
        &self.mountpoint
    }

    /// Unmounts the session and waits for all FUSE workers.
    ///
    /// # Errors
    ///
    /// Returns an error when the kernel mount cannot be released or a worker
    /// terminates with an error.
    pub fn unmount(mut self) -> ShareFsResult<()> {
        self.stop()
    }

    fn stop(&mut self) -> ShareFsResult<()> {
        let Some(session) = self.session.take() else {
            return Ok(());
        };
        std::fs::write(&self.connection_abort, b"1\n").map_err(|source| {
            Report::new(ShareFsError::Fuse {
                action: "abort the ShareFS FUSE connection",
                source,
            })
        })?;
        let session_result = session.umount_and_join();
        let detached = std::fs::metadata(&self.mountpoint)
            .is_ok_and(|metadata| metadata.dev() != self.connection_device);
        if !detached {
            return Err(Report::new(ShareFsError::Fuse {
                action: "unmount ShareFS",
                source: session_result.err().unwrap_or_else(|| {
                    std::io::Error::other("the FUSE mount remains attached after shutdown")
                }),
            }));
        }
        if let Err(source) = session_result {
            tracing::debug!(
                %source,
                mountpoint = %self.mountpoint.display(),
                "ShareFS worker stopped after its connection was aborted"
            );
        }
        Ok(())
    }
}

impl std::fmt::Debug for MountedShareFileSystem {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MountedShareFileSystem")
            .field("mountpoint", &self.mountpoint)
            .field("mounted", &self.session.is_some())
            .finish_non_exhaustive()
    }
}

impl Drop for MountedShareFileSystem {
    fn drop(&mut self) {
        if let Err(error) = self.stop() {
            warn!(%error, mountpoint = %self.mountpoint.display(), "could not unmount ShareFS");
        }
    }
}

struct FuseAdapter {
    filesystem: Arc<ShareFileSystem>,
    namespace: Mutex<InodeTable>,
    handles: Mutex<HandleTable>,
    uid: u32,
    gid: u32,
}

impl FuseAdapter {
    fn new(filesystem: Arc<ShareFileSystem>, uid: u32, gid: u32) -> Self {
        Self {
            filesystem,
            namespace: Mutex::new(InodeTable::new()),
            handles: Mutex::new(HandleTable::new()),
            uid,
            gid,
        }
    }

    fn path(&self, inode: INodeNo) -> Result<PathBuf, Errno> {
        self.namespace
            .lock()
            .map_err(|_| Errno::EIO)?
            .path(inode)
            .ok_or(Errno::ENOENT)
    }

    fn child(&self, parent: INodeNo, name: &OsStr) -> Result<PathBuf, Errno> {
        if name.is_empty() || name.as_encoded_bytes().contains(&b'/') {
            return Err(Errno::EINVAL);
        }
        Ok(self.path(parent)?.join(name))
    }

    fn inode(&self, path: &Path) -> Result<INodeNo, Errno> {
        self.namespace.lock().map_err(|_| Errno::EIO)?.inode(path)
    }

    fn attributes(&self, path: &Path, inode: INodeNo) -> Result<FileAttr, Errno> {
        let metadata = self
            .filesystem
            .metadata(path)
            .map_err(|error| share_errno(&error))?;
        Ok(file_attributes(&metadata, inode, self.uid, self.gid))
    }

    fn reply_entry(&self, path: &Path, reply: ReplyEntry) {
        match self.inode(path).and_then(|inode| {
            self.attributes(path, inode)
                .map(|attributes| (inode, attributes))
        }) {
            Ok((_inode, attributes)) => {
                reply.entry(&ATTRIBUTE_TTL, &attributes, Generation(0));
            }
            Err(error) => reply.error(error),
        }
    }

    fn handle_path(&self, handle: FileHandle, inode: INodeNo) -> Result<PathBuf, Errno> {
        let handles = self.handles.lock().map_err(|_| Errno::EIO)?;
        handles
            .file(handle)
            .map(|file| file.path.clone())
            .or_else(|| {
                drop(handles);
                self.path(inode).ok()
            })
            .ok_or(Errno::EBADF)
    }
}

impl Filesystem for FuseAdapter {
    fn init(&mut self, _request: &Request, config: &mut KernelConfig) -> std::io::Result<()> {
        config.set_max_write(1024 * 1024).map_err(|maximum| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("kernel limits ShareFS writes to {maximum} bytes"),
            )
        })?;
        Ok(())
    }

    fn lookup(&self, _request: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        match self.child(parent, name) {
            Ok(path) => self.reply_entry(&path, reply),
            Err(error) => reply.error(error),
        }
    }

    fn getattr(
        &self,
        _request: &Request,
        inode: INodeNo,
        _handle: Option<FileHandle>,
        reply: ReplyAttr,
    ) {
        match self
            .path(inode)
            .and_then(|path| self.attributes(&path, inode))
        {
            Ok(attributes) => reply.attr(&ATTRIBUTE_TTL, &attributes),
            Err(error) => reply.error(error),
        }
    }

    fn setattr(
        &self,
        _request: &Request,
        inode: INodeNo,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<TimeOrNow>,
        _mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        handle: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<fuser::BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        if uid.is_some_and(|uid| uid != self.uid) || gid.is_some_and(|gid| gid != self.gid) {
            reply.error(Errno::EPERM);
            return;
        }
        let path = match handle {
            Some(handle) => self.handle_path(handle, inode),
            None => self.path(inode),
        };
        let result = path.and_then(|path| {
            if let Some(size) = size {
                self.filesystem
                    .set_file_length(&path, size)
                    .map_err(|error| share_errno(&error))?;
            }
            if let Some(mode) = mode {
                self.filesystem
                    .set_mode(&path, mode)
                    .map_err(|error| share_errno(&error))?;
            }
            self.attributes(&path, inode)
        });
        match result {
            Ok(attributes) => reply.attr(&ATTRIBUTE_TTL, &attributes),
            Err(error) => reply.error(error),
        }
    }

    fn readlink(&self, _request: &Request, inode: INodeNo, reply: ReplyData) {
        match self.path(inode).and_then(|path| {
            self.filesystem
                .read_link(path)
                .map_err(|error| share_errno(&error))
        }) {
            Ok(target) => reply.data(target.as_os_str().as_encoded_bytes()),
            Err(error) => reply.error(error),
        }
    }

    fn mkdir(
        &self,
        _request: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        reply: ReplyEntry,
    ) {
        match self.child(parent, name).and_then(|path| {
            self.filesystem
                .create_directory(&path, mode & !umask)
                .map_err(|error| share_errno(&error))?;
            Ok(path)
        }) {
            Ok(path) => self.reply_entry(&path, reply),
            Err(error) => reply.error(error),
        }
    }

    fn unlink(&self, _request: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        self.remove(parent, name, false, reply);
    }

    fn rmdir(&self, _request: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        self.remove(parent, name, true, reply);
    }

    fn symlink(
        &self,
        _request: &Request,
        parent: INodeNo,
        link_name: &OsStr,
        target: &Path,
        reply: ReplyEntry,
    ) {
        match self.child(parent, link_name).and_then(|path| {
            self.filesystem
                .create_symlink(&path, target)
                .map_err(|error| share_errno(&error))?;
            Ok(path)
        }) {
            Ok(path) => self.reply_entry(&path, reply),
            Err(error) => reply.error(error),
        }
    }

    fn rename(
        &self,
        _request: &Request,
        parent: INodeNo,
        name: &OsStr,
        new_parent: INodeNo,
        new_name: &OsStr,
        flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        if !flags.is_empty() {
            reply.error(Errno::EOPNOTSUPP);
            return;
        }
        let result = self.child(parent, name).and_then(|source| {
            let destination = self.child(new_parent, new_name)?;
            self.filesystem
                .rename(&source, &destination)
                .map_err(|error| share_errno(&error))?;
            self.namespace
                .lock()
                .map_err(|_| Errno::EIO)?
                .renamed(&source, &destination);
            self.handles
                .lock()
                .map_err(|_| Errno::EIO)?
                .renamed(&source, &destination);
            Ok(())
        });
        match result {
            Ok(()) => reply.ok(),
            Err(error) => reply.error(error),
        }
    }

    fn open(&self, _request: &Request, inode: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let result = self.path(inode).and_then(|path| {
            let metadata = self
                .filesystem
                .metadata(&path)
                .map_err(|error| share_errno(&error))?;
            if metadata.kind != EntryKind::File {
                return Err(Errno::EISDIR);
            }
            if flags.0 & libc::O_TRUNC != 0 {
                self.filesystem
                    .set_file_length(&path, 0)
                    .map_err(|error| share_errno(&error))?;
            }
            self.handles.lock().map_err(|_| Errno::EIO)?.open_file(path)
        });
        match result {
            Ok(handle) => reply.opened(handle, FopenFlags::FOPEN_DIRECT_IO),
            Err(error) => reply.error(error),
        }
    }

    fn read(
        &self,
        _request: &Request,
        inode: INodeNo,
        handle: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        reply: ReplyData,
    ) {
        let result = self.handle_path(handle, inode).and_then(|path| {
            let contents = self
                .filesystem
                .read_file(path)
                .map_err(|error| share_errno(&error))?;
            let start = usize::try_from(offset)
                .unwrap_or(usize::MAX)
                .min(contents.len());
            let end = start.saturating_add(size as usize).min(contents.len());
            Ok(contents[start..end].to_vec())
        });
        match result {
            Ok(contents) => reply.data(&contents),
            Err(error) => reply.error(error),
        }
    }

    fn write(
        &self,
        _request: &Request,
        inode: INodeNo,
        handle: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: fuser::WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        reply: ReplyWrite,
    ) {
        let result = self.handle_path(handle, inode).and_then(|path| {
            self.filesystem
                .write_at(path, offset, data)
                .map_err(|error| share_errno(&error))?;
            u32::try_from(data.len()).map_err(|_| Errno::EFBIG)
        });
        match result {
            Ok(written) => reply.written(written),
            Err(error) => reply.error(error),
        }
    }

    fn flush(
        &self,
        _request: &Request,
        _inode: INodeNo,
        _handle: FileHandle,
        _lock_owner: fuser::LockOwner,
        reply: ReplyEmpty,
    ) {
        match self.filesystem.sync() {
            Ok(()) => reply.ok(),
            Err(error) => reply.error(share_errno(&error)),
        }
    }

    fn release(
        &self,
        _request: &Request,
        _inode: INodeNo,
        handle: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        match self.handles.lock() {
            Ok(mut handles) => {
                handles.close(handle);
                reply.ok();
            }
            Err(_) => reply.error(Errno::EIO),
        }
    }

    fn fsync(
        &self,
        _request: &Request,
        _inode: INodeNo,
        _handle: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        match self.filesystem.sync() {
            Ok(()) => reply.ok(),
            Err(error) => reply.error(share_errno(&error)),
        }
    }

    fn opendir(&self, _request: &Request, inode: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let result = self.path(inode).and_then(|path| {
            let entries = self
                .filesystem
                .read_directory(&path)
                .map_err(|error| share_errno(&error))?;
            let parent = path.parent().unwrap_or(Path::new("")).to_owned();
            self.handles
                .lock()
                .map_err(|_| Errno::EIO)?
                .open_directory(path, parent, entries, flags)
        });
        match result {
            Ok(handle) => reply.opened(handle, FopenFlags::empty()),
            Err(error) => reply.error(error),
        }
    }

    fn readdir(
        &self,
        _request: &Request,
        _inode: INodeNo,
        handle: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let result = (|| {
            let handles = self.handles.lock().map_err(|_| Errno::EIO)?;
            let directory = handles.directory(handle).ok_or(Errno::EBADF)?;
            let mut entries = Vec::with_capacity(directory.entries.len() + 2);
            entries.push((
                self.inode(&directory.path)?,
                FileType::Directory,
                OsStr::new(".").to_owned(),
            ));
            entries.push((
                self.inode(&directory.parent)?,
                FileType::Directory,
                OsStr::new("..").to_owned(),
            ));
            for entry in &directory.entries {
                let path = directory.path.join(&entry.name);
                entries.push((
                    self.inode(&path)?,
                    file_type(entry.metadata.kind),
                    entry.name.clone(),
                ));
            }
            Ok::<_, Errno>(entries)
        })();
        let entries = match result {
            Ok(entries) => entries,
            Err(error) => {
                reply.error(error);
                return;
            }
        };
        let offset = usize::try_from(offset).unwrap_or(usize::MAX);
        for (index, (entry_inode, kind, name)) in entries.into_iter().enumerate().skip(offset) {
            if reply.add(entry_inode, (index + 1) as u64, kind, name) {
                break;
            }
        }
        reply.ok();
    }

    fn releasedir(
        &self,
        _request: &Request,
        _inode: INodeNo,
        handle: FileHandle,
        _flags: OpenFlags,
        reply: ReplyEmpty,
    ) {
        match self.handles.lock() {
            Ok(mut handles) => {
                handles.close(handle);
                reply.ok();
            }
            Err(_) => reply.error(Errno::EIO),
        }
    }

    fn fsyncdir(
        &self,
        _request: &Request,
        _inode: INodeNo,
        _handle: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        match self.filesystem.sync() {
            Ok(()) => reply.ok(),
            Err(error) => reply.error(share_errno(&error)),
        }
    }

    fn create(
        &self,
        _request: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        let result = self.child(parent, name).and_then(|path| {
            self.filesystem
                .create_file(&path, mode & !umask)
                .map_err(|error| share_errno(&error))?;
            let inode = self.inode(&path)?;
            let attributes = self.attributes(&path, inode)?;
            let handle = self
                .handles
                .lock()
                .map_err(|_| Errno::EIO)?
                .open_file(path)?;
            Ok((attributes, handle))
        });
        match result {
            Ok((attributes, handle)) => reply.created(
                &ATTRIBUTE_TTL,
                &attributes,
                Generation(0),
                handle,
                FopenFlags::FOPEN_DIRECT_IO,
            ),
            Err(error) => reply.error(error),
        }
    }
}

impl FuseAdapter {
    fn remove(&self, parent: INodeNo, name: &OsStr, directory: bool, reply: ReplyEmpty) {
        let result = self.child(parent, name).and_then(|path| {
            let metadata = self
                .filesystem
                .metadata(&path)
                .map_err(|error| share_errno(&error))?;
            if directory != (metadata.kind == EntryKind::Directory) {
                return Err(if directory {
                    Errno::ENOTDIR
                } else {
                    Errno::EISDIR
                });
            }
            self.filesystem
                .remove(&path)
                .map_err(|error| share_errno(&error))?;
            Ok(path)
        });
        match result {
            Ok(path) => {
                match self.namespace.lock() {
                    Ok(mut namespace) => namespace.removed(&path),
                    Err(_) => {
                        warn!(
                            path = %path.display(),
                            "could not update the poisoned ShareFS inode table after removal"
                        );
                    }
                }
                reply.ok();
            }
            Err(error) => reply.error(error),
        }
    }
}

struct InodeTable {
    next_inode: u64,
    by_inode: BTreeMap<u64, PathBuf>,
    by_path: HashMap<PathBuf, u64>,
}

impl InodeTable {
    fn new() -> Self {
        let root = PathBuf::new();
        Self {
            next_inode: ROOT_INODE + 1,
            by_inode: BTreeMap::from([(ROOT_INODE, root.clone())]),
            by_path: HashMap::from([(root, ROOT_INODE)]),
        }
    }

    fn path(&self, inode: INodeNo) -> Option<PathBuf> {
        self.by_inode.get(&inode.0).cloned()
    }

    fn inode(&mut self, path: &Path) -> Result<INodeNo, Errno> {
        if let Some(inode) = self.by_path.get(path) {
            return Ok(INodeNo(*inode));
        }
        let inode = self.next_inode;
        self.next_inode = self.next_inode.checked_add(1).ok_or(Errno::EOVERFLOW)?;
        let path = path.to_owned();
        self.by_path.insert(path.clone(), inode);
        self.by_inode.insert(inode, path);
        Ok(INodeNo(inode))
    }

    fn removed(&mut self, path: &Path) {
        let removed = self
            .by_path
            .keys()
            .filter(|candidate| *candidate == path || candidate.starts_with(path))
            .cloned()
            .collect::<Vec<_>>();
        for path in removed {
            if let Some(inode) = self.by_path.remove(&path) {
                self.by_inode.remove(&inode);
            }
        }
    }

    fn renamed(&mut self, source: &Path, destination: &Path) {
        let renamed = self
            .by_path
            .iter()
            .filter(|(path, _)| *path == source || path.starts_with(source))
            .map(|(path, inode)| (path.clone(), *inode))
            .collect::<Vec<_>>();
        for (path, inode) in renamed {
            let suffix = path.strip_prefix(source).unwrap_or(Path::new(""));
            let replacement = destination.join(suffix);
            self.by_path.remove(&path);
            self.by_path.insert(replacement.clone(), inode);
            self.by_inode.insert(inode, replacement);
        }
    }
}

struct HandleTable {
    next_handle: u64,
    handles: BTreeMap<u64, Handle>,
}

impl HandleTable {
    fn new() -> Self {
        Self {
            next_handle: 1,
            handles: BTreeMap::new(),
        }
    }

    fn open_file(&mut self, path: PathBuf) -> Result<FileHandle, Errno> {
        let handle = self.allocate()?;
        self.handles
            .insert(handle.0, Handle::File(OpenFile { path }));
        Ok(handle)
    }

    fn open_directory(
        &mut self,
        path: PathBuf,
        parent: PathBuf,
        entries: Vec<DirectoryEntry>,
        flags: OpenFlags,
    ) -> Result<FileHandle, Errno> {
        let handle = self.allocate()?;
        self.handles.insert(
            handle.0,
            Handle::Directory(OpenDirectory {
                path,
                parent,
                entries,
                _flags: flags,
            }),
        );
        Ok(handle)
    }

    fn file(&self, handle: FileHandle) -> Option<&OpenFile> {
        match self.handles.get(&handle.0) {
            Some(Handle::File(file)) => Some(file),
            Some(Handle::Directory(_)) | None => None,
        }
    }

    fn directory(&self, handle: FileHandle) -> Option<&OpenDirectory> {
        match self.handles.get(&handle.0) {
            Some(Handle::Directory(directory)) => Some(directory),
            Some(Handle::File(_)) | None => None,
        }
    }

    fn close(&mut self, handle: FileHandle) {
        self.handles.remove(&handle.0);
    }

    fn renamed(&mut self, source: &Path, destination: &Path) {
        for handle in self.handles.values_mut() {
            let path = match handle {
                Handle::File(file) => &mut file.path,
                Handle::Directory(directory) => &mut directory.path,
            };
            if *path == source || path.starts_with(source) {
                let suffix = path.strip_prefix(source).unwrap_or(Path::new(""));
                *path = destination.join(suffix);
            }
        }
    }

    fn allocate(&mut self) -> Result<FileHandle, Errno> {
        let handle = self.next_handle;
        self.next_handle = self.next_handle.checked_add(1).ok_or(Errno::EMFILE)?;
        Ok(FileHandle(handle))
    }
}

enum Handle {
    File(OpenFile),
    Directory(OpenDirectory),
}

struct OpenFile {
    path: PathBuf,
}

struct OpenDirectory {
    path: PathBuf,
    parent: PathBuf,
    entries: Vec<DirectoryEntry>,
    _flags: OpenFlags,
}

fn file_attributes(metadata: &EntryMetadata, inode: INodeNo, uid: u32, gid: u32) -> FileAttr {
    let modified = system_time(
        metadata.modified_at.seconds,
        metadata.modified_at.nanoseconds,
    );
    let permission_floor = match metadata.kind {
        EntryKind::File => 0o600,
        EntryKind::Directory => 0o700,
        EntryKind::Symlink => 0o777,
    };
    FileAttr {
        ino: inode,
        size: metadata.size,
        blocks: metadata.size.div_ceil(512),
        atime: modified,
        mtime: modified,
        ctime: modified,
        crtime: modified,
        kind: file_type(metadata.kind),
        perm: ((metadata.mode | permission_floor) & 0o7777) as u16,
        nlink: if metadata.kind == EntryKind::Directory {
            2
        } else {
            1
        },
        uid,
        gid,
        rdev: 0,
        blksize: BLOCK_SIZE,
        flags: 0,
    }
}

fn file_type(kind: EntryKind) -> FileType {
    match kind {
        EntryKind::File => FileType::RegularFile,
        EntryKind::Directory => FileType::Directory,
        EntryKind::Symlink => FileType::Symlink,
    }
}

fn system_time(seconds: i64, nanoseconds: u32) -> SystemTime {
    let duration = Duration::new(seconds.unsigned_abs(), nanoseconds);
    if seconds >= 0 {
        UNIX_EPOCH.checked_add(duration).unwrap_or(UNIX_EPOCH)
    } else {
        UNIX_EPOCH.checked_sub(duration).unwrap_or(UNIX_EPOCH)
    }
}

fn share_errno(error: &Report<ShareFsError>) -> Errno {
    match error.error() {
        ShareFsError::NotFound { .. } => Errno::ENOENT,
        ShareFsError::AlreadyExists { .. } => Errno::EEXIST,
        ShareFsError::NotDirectory { .. } => Errno::ENOTDIR,
        ShareFsError::IsDirectory { .. } => Errno::EISDIR,
        ShareFsError::DirectoryNotEmpty { .. } => Errno::ENOTEMPTY,
        ShareFsError::InvalidPath { .. } => Errno::EINVAL,
        ShareFsError::UnsupportedEntryType { .. } | ShareFsError::LowerDirectoryRename { .. } => {
            Errno::EOPNOTSUPP
        }
        ShareFsError::ConcurrentLowerChange { .. } => Errno::EAGAIN,
        ShareFsError::InvalidLowerDirectory { .. }
        | ShareFsError::InvalidStateDirectory { .. }
        | ShareFsError::OverlappingDirectories
        | ShareFsError::StateInUse
        | ShareFsError::CorruptState
        | ShareFsError::Io { .. }
        | ShareFsError::Database { .. }
        | ShareFsError::Fuse { .. }
        | ShareFsError::Poisoned => Errno::EIO,
    }
}
