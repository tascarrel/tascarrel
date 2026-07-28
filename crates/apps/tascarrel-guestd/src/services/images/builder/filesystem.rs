//! Filesystem validation and bounded file publication for image builds.

use super::DirBuilderExt;
use super::File;
use super::ID_MAP_SIZE;
use super::ImageBuildError;
use super::ImageBuildLimits;
use super::MAX_OCI_METADATA_BYTES;
use super::MAX_SYMLINK_TARGET_BYTES;
use super::MetadataExt;
use super::OpenOptions;
use super::OpenOptionsExt;
use super::OsStr;
use super::OsStrExt;
use super::Path;
use super::PathBuf;
use super::Read;
use super::Write;
use super::fs;

pub(crate) fn read_bounded_metadata(
    path: &Path,
    kind: &'static str,
) -> Result<Vec<u8>, ImageBuildError> {
    let metadata = real_regular_file(path, kind)?;
    if metadata.len() > MAX_OCI_METADATA_BYTES {
        return Err(ImageBuildError::UnsafeOutput {
            kind,
            path: path.to_path_buf(),
            reason: "metadata document is too large",
        });
    }
    let file = File::open(path).map_err(|source| ImageBuildError::Io {
        operation: "open OCI metadata",
        path: path.to_path_buf(),
        source,
    })?;
    let mut contents = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or(0));
    file.take(MAX_OCI_METADATA_BYTES + 1)
        .read_to_end(&mut contents)
        .map_err(|source| ImageBuildError::Io {
            operation: "read OCI metadata",
            path: path.to_path_buf(),
            source,
        })?;
    if u64::try_from(contents.len()).unwrap_or(u64::MAX) > MAX_OCI_METADATA_BYTES {
        return Err(ImageBuildError::UnsafeOutput {
            kind,
            path: path.to_path_buf(),
            reason: "metadata document grew while it was read",
        });
    }
    Ok(contents)
}

pub(crate) fn validate_umoci_bundle(
    bundle: &Path,
    limits: &ImageBuildLimits,
) -> Result<PathBuf, ImageBuildError> {
    real_directory(bundle, "umoci bundle")?;
    real_regular_file(&bundle.join("config.json"), "umoci config")?;
    let rootfs = bundle.join("rootfs");
    validate_tree(
        &rootfs,
        TreePolicy {
            kind: "unpacked root filesystem",
            allow_symlinks: true,
            allow_hardlinks: true,
            require_mapped_ownership: true,
        },
        limits,
    )?;
    Ok(rootfs)
}

#[derive(Clone, Copy)]
pub(crate) struct TreePolicy {
    pub(crate) kind: &'static str,
    pub(crate) allow_symlinks: bool,
    pub(crate) allow_hardlinks: bool,
    pub(crate) require_mapped_ownership: bool,
}

pub(crate) fn validate_tree(
    root: &Path,
    policy: TreePolicy,
    limits: &ImageBuildLimits,
) -> Result<(), ImageBuildError> {
    let root_metadata = real_directory(root, policy.kind)?;
    validate_tree_ownership(root, &root_metadata, policy)?;
    let mut state = TreeState {
        root,
        device: root_metadata.dev(),
        policy,
        limits,
        entries: 0,
        bytes: 0,
    };
    state.walk(Path::new(""), 0)
}

struct TreeState<'a> {
    root: &'a Path,
    device: u64,
    policy: TreePolicy,
    limits: &'a ImageBuildLimits,
    entries: u64,
    bytes: u64,
}

impl TreeState<'_> {
    #[allow(clippy::too_many_lines)] // Keeping lstat/type checks together makes links auditable.
    fn walk(&mut self, relative: &Path, depth: usize) -> Result<(), ImageBuildError> {
        if depth > self.limits.max_output_depth {
            return Err(ImageBuildError::OutputLimit {
                kind: self.policy.kind,
                path: self.root.join(relative),
                limit: "maximum directory depth",
            });
        }
        let directory = self.root.join(relative);
        let metadata = fs::symlink_metadata(&directory).map_err(|source| ImageBuildError::Io {
            operation: "inspect output directory",
            path: directory.clone(),
            source,
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() || metadata.dev() != self.device
        {
            return Err(ImageBuildError::UnsafeOutput {
                kind: self.policy.kind,
                path: directory,
                reason: "directory is a link, special entry, or filesystem boundary",
            });
        }
        validate_tree_ownership(&directory, &metadata, self.policy)?;
        let mut children = fs::read_dir(&directory)
            .map_err(|source| ImageBuildError::Io {
                operation: "read output directory",
                path: directory.clone(),
                source,
            })?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|source| ImageBuildError::Io {
                operation: "read output directory entry",
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
                return Err(ImageBuildError::UnsafeOutput {
                    kind: self.policy.kind,
                    path: child.path(),
                    reason: "entry name is not a safe path component",
                });
            }
            let child_relative = relative.join(name);
            let path = self.root.join(&child_relative);
            let metadata = fs::symlink_metadata(&path).map_err(|source| ImageBuildError::Io {
                operation: "inspect output entry",
                path: path.clone(),
                source,
            })?;
            if metadata.dev() != self.device {
                return Err(ImageBuildError::UnsafeOutput {
                    kind: self.policy.kind,
                    path,
                    reason: "entry crosses a filesystem boundary",
                });
            }
            validate_tree_ownership(&path, &metadata, self.policy)?;
            self.entries =
                self.entries
                    .checked_add(1)
                    .ok_or_else(|| ImageBuildError::OutputLimit {
                        kind: self.policy.kind,
                        path: path.clone(),
                        limit: "maximum entry count",
                    })?;
            if self.entries > self.limits.max_output_entries {
                return Err(ImageBuildError::OutputLimit {
                    kind: self.policy.kind,
                    path,
                    limit: "maximum entry count",
                });
            }

            if metadata.is_dir() {
                self.walk(&child_relative, depth + 1)?;
            } else if metadata.is_file() {
                if !self.policy.allow_hardlinks && metadata.nlink() != 1 {
                    return Err(ImageBuildError::UnsafeOutput {
                        kind: self.policy.kind,
                        path,
                        reason: "hard-linked files are not accepted",
                    });
                }
                self.bytes = self.bytes.checked_add(metadata.len()).ok_or_else(|| {
                    ImageBuildError::OutputLimit {
                        kind: self.policy.kind,
                        path: path.clone(),
                        limit: "maximum aggregate file bytes",
                    }
                })?;
                if self.bytes > self.limits.max_output_bytes {
                    return Err(ImageBuildError::OutputLimit {
                        kind: self.policy.kind,
                        path,
                        limit: "maximum aggregate file bytes",
                    });
                }
            } else if metadata.file_type().is_symlink() && self.policy.allow_symlinks {
                let target = fs::read_link(&path).map_err(|source| ImageBuildError::Io {
                    operation: "read output symlink",
                    path: path.clone(),
                    source,
                })?;
                if target.as_os_str().as_bytes().len() > MAX_SYMLINK_TARGET_BYTES {
                    return Err(ImageBuildError::UnsafeOutput {
                        kind: self.policy.kind,
                        path,
                        reason: "symlink target is too long",
                    });
                }
            } else {
                return Err(ImageBuildError::UnsafeOutput {
                    kind: self.policy.kind,
                    path,
                    reason: "devices, sockets, FIFOs, and disallowed links are not accepted",
                });
            }
        }
        Ok(())
    }
}

pub(crate) fn validate_tree_ownership(
    path: &Path,
    metadata: &fs::Metadata,
    policy: TreePolicy,
) -> Result<(), ImageBuildError> {
    if policy.require_mapped_ownership
        && (metadata.uid() >= ID_MAP_SIZE || metadata.gid() >= ID_MAP_SIZE)
    {
        return Err(ImageBuildError::UnsafeOutput {
            kind: policy.kind,
            path: path.to_path_buf(),
            reason: "entry owner is outside the pod user-namespace map",
        });
    }
    Ok(())
}

pub(crate) fn real_directory(
    path: &Path,
    kind: &'static str,
) -> Result<fs::Metadata, ImageBuildError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| ImageBuildError::Io {
        operation: "inspect output directory",
        path: path.to_path_buf(),
        source,
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(ImageBuildError::UnsafeOutput {
            kind,
            path: path.to_path_buf(),
            reason: "expected a real directory",
        });
    }
    Ok(metadata)
}

pub(crate) fn real_regular_file(
    path: &Path,
    kind: &'static str,
) -> Result<fs::Metadata, ImageBuildError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| ImageBuildError::Io {
        operation: "inspect output file",
        path: path.to_path_buf(),
        source,
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(ImageBuildError::UnsafeOutput {
            kind,
            path: path.to_path_buf(),
            reason: "expected a real regular file",
        });
    }
    Ok(metadata)
}

pub(crate) fn ensure_empty_real_directory(
    path: &Path,
    kind: &'static str,
) -> Result<(), ImageBuildError> {
    real_directory(path, kind)?;
    if fs::read_dir(path)
        .map_err(|source| ImageBuildError::Io {
            operation: "read image staging directory",
            path: path.to_path_buf(),
            source,
        })?
        .next()
        .is_some()
    {
        return Err(ImageBuildError::UnsafeOutput {
            kind,
            path: path.to_path_buf(),
            reason: "directory is not empty",
        });
    }
    Ok(())
}

pub(crate) fn create_private_directory(path: &Path) -> Result<(), ImageBuildError> {
    let mut builder = fs::DirBuilder::new();
    builder
        .mode(0o700)
        .create(path)
        .map_err(|source| ImageBuildError::Io {
            operation: "create private build subdirectory",
            path: path.to_path_buf(),
            source,
        })
}

pub(crate) fn write_private_file(path: &Path, contents: &[u8]) -> Result<(), ImageBuildError> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|source| ImageBuildError::Io {
            operation: "create private build configuration",
            path: path.to_path_buf(),
            source,
        })?;
    file.write_all(contents)
        .and_then(|()| file.sync_all())
        .map_err(|source| ImageBuildError::Io {
            operation: "write private build configuration",
            path: path.to_path_buf(),
            source,
        })
}

pub(crate) fn safe_component(component: &OsStr) -> bool {
    let bytes = component.as_bytes();
    !bytes.is_empty() && bytes != b"." && bytes != b".." && !bytes.contains(&0)
}
