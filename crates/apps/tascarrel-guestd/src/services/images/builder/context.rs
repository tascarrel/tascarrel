//! Safe deterministic hashing of Dockerfile build contexts.

use super::*;

#[derive(Clone, Debug)]
pub(crate) struct ContextSnapshot {
    pub(crate) root: PathBuf,
    pub(crate) image: ImageId,
}

pub(crate) fn hash_context(
    context: &Path,
    limits: &ImageBuildLimits,
) -> Result<ContextSnapshot, ImageBuildError> {
    let metadata = fs::symlink_metadata(context).map_err(|source| ImageBuildError::Io {
        operation: "inspect image context",
        path: context.to_path_buf(),
        source,
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(ImageBuildError::UnsafeContext {
            path: context.to_path_buf(),
            reason: "context root is not a real directory",
        });
    }
    let root = fs::canonicalize(context).map_err(|source| ImageBuildError::Io {
        operation: "canonicalize image context",
        path: context.to_path_buf(),
        source,
    })?;
    let mut walker = ContextWalker {
        root: &root,
        device: metadata.dev(),
        limits,
        entries: 0,
        bytes: 0,
        dockerfile: false,
        hasher: Sha256::new(),
    };
    walker.hasher.update(HASH_DOMAIN);
    walker.walk(Path::new(""), 0)?;
    if !walker.dockerfile {
        return Err(ImageBuildError::UnsafeContext {
            path: root.join(DOCKERFILE),
            reason: "root-level Dockerfile is missing or not a regular file",
        });
    }
    let encoded = format!("{:x}", walker.hasher.finalize());
    let image =
        ImageId::new(format!("sha256:{encoded}")).map_err(|_| ImageBuildError::UnsafeContext {
            path: root.clone(),
            reason: "generated digest was unexpectedly invalid",
        })?;
    Ok(ContextSnapshot { root, image })
}

struct ContextWalker<'a> {
    root: &'a Path,
    device: u64,
    limits: &'a ImageBuildLimits,
    entries: u64,
    bytes: u64,
    dockerfile: bool,
    hasher: Sha256,
}

impl ContextWalker<'_> {
    #[allow(clippy::too_many_lines)] // Validate each entry at the point it is hashed.
    fn walk(&mut self, relative: &Path, depth: usize) -> Result<(), ImageBuildError> {
        if depth > self.limits.max_context_depth {
            return Err(ImageBuildError::ContextLimit {
                path: self.root.join(relative),
                limit: "maximum directory depth",
            });
        }
        let directory = self.root.join(relative);
        let before = fs::symlink_metadata(&directory).map_err(|source| ImageBuildError::Io {
            operation: "inspect context directory",
            path: directory.clone(),
            source,
        })?;
        if before.file_type().is_symlink() || !before.is_dir() || before.dev() != self.device {
            return Err(ImageBuildError::UnsafeContext {
                path: directory,
                reason: "directory is a link, special entry, or filesystem boundary",
            });
        }

        let mut children = fs::read_dir(&directory)
            .map_err(|source| ImageBuildError::Io {
                operation: "read context directory",
                path: directory.clone(),
                source,
            })?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|source| ImageBuildError::Io {
                operation: "read context directory entry",
                path: directory.clone(),
                source,
            })?;
        children.sort_by(|left, right| {
            left.file_name()
                .as_bytes()
                .cmp(right.file_name().as_bytes())
        });

        for child in children {
            let name = child.file_name();
            if !safe_component(&name) {
                return Err(ImageBuildError::UnsafeContext {
                    path: child.path(),
                    reason: "entry name is not a safe path component",
                });
            }
            let child_relative = relative.join(&name);
            let path = self.root.join(&child_relative);
            let metadata = fs::symlink_metadata(&path).map_err(|source| ImageBuildError::Io {
                operation: "inspect context entry",
                path: path.clone(),
                source,
            })?;
            if metadata.dev() != self.device {
                return Err(ImageBuildError::UnsafeContext {
                    path,
                    reason: "entry crosses a filesystem boundary",
                });
            }
            self.entries =
                self.entries
                    .checked_add(1)
                    .ok_or_else(|| ImageBuildError::ContextLimit {
                        path: path.clone(),
                        limit: "maximum entry count",
                    })?;
            if self.entries > self.limits.max_context_entries {
                return Err(ImageBuildError::ContextLimit {
                    path,
                    limit: "maximum entry count",
                });
            }

            if metadata.is_dir() {
                hash_entry_header(&mut self.hasher, b'd', &child_relative, &metadata, 0);
                self.walk(&child_relative, depth + 1)?;
            } else if metadata.is_file() {
                if metadata.nlink() != 1 {
                    return Err(ImageBuildError::UnsafeContext {
                        path,
                        reason: "hard-linked context files are not accepted",
                    });
                }
                if child_relative.as_os_str().as_bytes() == DOCKERFILE.as_bytes() {
                    self.dockerfile = true;
                }
                self.hash_file(&child_relative, &metadata)?;
            } else {
                return Err(ImageBuildError::UnsafeContext {
                    path,
                    reason: "links, devices, sockets, and FIFOs are not accepted",
                });
            }
        }

        let after = fs::symlink_metadata(&directory).map_err(|source| ImageBuildError::Io {
            operation: "reinspect context directory",
            path: directory.clone(),
            source,
        })?;
        if !same_metadata(&before, &after) {
            return Err(ImageBuildError::UnsafeContext {
                path: directory,
                reason: "directory changed while it was being hashed",
            });
        }
        Ok(())
    }

    fn hash_file(
        &mut self,
        relative: &Path,
        expected: &fs::Metadata,
    ) -> Result<(), ImageBuildError> {
        let path = self.root.join(relative);
        self.bytes = self.bytes.checked_add(expected.len()).ok_or_else(|| {
            ImageBuildError::ContextLimit {
                path: path.clone(),
                limit: "maximum aggregate file bytes",
            }
        })?;
        if self.bytes > self.limits.max_context_bytes {
            return Err(ImageBuildError::ContextLimit {
                path,
                limit: "maximum aggregate file bytes",
            });
        }
        hash_entry_header(&mut self.hasher, b'f', relative, expected, expected.len());

        let descriptor = open(
            &path,
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .map_err(|source| ImageBuildError::Io {
            operation: "open context file without following links",
            path: path.clone(),
            source: io::Error::from_raw_os_error(source.raw_os_error()),
        })?;
        let mut file = File::from(descriptor);
        let opened = file.metadata().map_err(|source| ImageBuildError::Io {
            operation: "inspect open context file",
            path: path.clone(),
            source,
        })?;
        if !same_metadata(expected, &opened) {
            return Err(ImageBuildError::UnsafeContext {
                path,
                reason: "file changed before it could be opened",
            });
        }

        let mut read = 0_u64;
        let mut buffer = vec![0_u8; READ_BUFFER_SIZE].into_boxed_slice();
        loop {
            let count = file
                .read(&mut buffer)
                .map_err(|source| ImageBuildError::Io {
                    operation: "hash context file",
                    path: path.clone(),
                    source,
                })?;
            if count == 0 {
                break;
            }
            read = read
                .checked_add(u64::try_from(count).unwrap_or(u64::MAX))
                .ok_or_else(|| ImageBuildError::UnsafeContext {
                    path: path.clone(),
                    reason: "file size changed while it was being hashed",
                })?;
            if read > expected.len() {
                return Err(ImageBuildError::UnsafeContext {
                    path,
                    reason: "file grew while it was being hashed",
                });
            }
            self.hasher.update(&buffer[..count]);
        }
        let after = file.metadata().map_err(|source| ImageBuildError::Io {
            operation: "reinspect context file",
            path: path.clone(),
            source,
        })?;
        if read != expected.len() || !same_metadata(expected, &after) {
            return Err(ImageBuildError::UnsafeContext {
                path,
                reason: "file changed while it was being hashed",
            });
        }
        Ok(())
    }
}

fn hash_entry_header(
    hasher: &mut Sha256,
    entry_type: u8,
    relative: &Path,
    metadata: &fs::Metadata,
    size: u64,
) {
    let path = relative.as_os_str().as_bytes();
    hasher.update([entry_type]);
    hasher.update(u64::try_from(path.len()).unwrap_or(u64::MAX).to_le_bytes());
    hasher.update(path);
    hasher.update((metadata.mode() & 0o7777).to_le_bytes());
    hasher.update(size.to_le_bytes());
}

pub(crate) fn same_metadata(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    left.dev() == right.dev()
        && left.ino() == right.ino()
        && left.mode() == right.mode()
        && left.len() == right.len()
        && left.mtime() == right.mtime()
        && left.mtime_nsec() == right.mtime_nsec()
}
