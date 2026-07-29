//! Guest mounting for host directories pinned to one managed workspace VM.
//!
//! Raw virtiofs or virtio-9p exports stay below the private guest runtime.
//! Bindfs supplies a stable ownership-normalized view at
//! `/mnt/shares/<name>` that can be idmapped into every pod. This is required
//! even for virtiofs: each pod has a distinct outer identity range, while host
//! files retain their host ownership and modes.

use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::DirBuilderExt as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;

use anyhow::Context as _;
use anyhow::Result;
use anyhow::anyhow;
use reportify::ErrorExt as _;
use reportify::Report;
use tascarrel_protocol::WorkspaceHostShare;
use tascarrel_protocol::WorkspaceHostSharesResponse;
use thiserror::Error;
use tracing::debug;
use tracing::warn;

/// Mounted host shares owned by the guest daemon.
#[derive(Debug)]
pub(crate) struct HostShareMounts {
    mounts: Vec<MountedHostShare>,
    umount_program: PathBuf,
}

impl HostShareMounts {
    /// Mounts every validated host-pinned share.
    ///
    /// # Errors
    ///
    /// Returns an error when directories are unsafe, neither shared-directory
    /// transport can be mounted, or the ownership bridge cannot start.
    #[tracing::instrument(
        name = "tascarrel_guest.host_shares.mount",
        level = "info",
        skip_all,
        fields(shares = manifest.shares.len()),
        err
    )]
    pub(crate) fn mount(
        manifest: &WorkspaceHostSharesResponse,
        mount_program: &Path,
        umount_program: &Path,
        bindfs_program: &Path,
        runtime_directory: &Path,
    ) -> std::result::Result<Self, Report<HostShareMountError>> {
        manifest
            .validate()
            .map_err(|error| error.escalate(HostShareMountError::InvalidManifest))?;
        ensure_directory(Path::new(HOST_SHARES_DIRECTORY), 0o755)
            .map_err(|error| mount_error(&error))?;
        let raw_root = runtime_directory.join("host-shares");
        ensure_directory(&raw_root, 0o700).map_err(|error| mount_error(&error))?;
        let mut mounted = Self {
            mounts: Vec::with_capacity(manifest.shares.len()),
            umount_program: umount_program.to_owned(),
        };
        for share in &manifest.shares {
            if let Err(error) = mounted.mount_one(share, mount_program, bindfs_program, &raw_root) {
                mounted.unmount_best_effort();
                return Err(mount_error(&error));
            }
        }
        Ok(mounted)
    }

    /// Unmounts all ownership bridges and shared-directory transports.
    ///
    /// # Errors
    ///
    /// Returns the first cleanup error after attempting every mount.
    pub(crate) fn unmount(&mut self) -> std::result::Result<(), Report<HostShareMountError>> {
        let mut first_error = None;
        for mounted in self.mounts.drain(..).rev() {
            for path in [&mounted.exposed, &mounted.raw] {
                if let Err(error) = unmount_if_mounted(&self.umount_program, path)
                    && first_error.is_none()
                {
                    first_error = Some(error);
                }
            }
        }
        first_error.map_or(Ok(()), |error| Err(unmount_error(&error)))
    }

    fn mount_one(
        &mut self,
        share: &WorkspaceHostShare,
        mount_program: &Path,
        bindfs_program: &Path,
        raw_root: &Path,
    ) -> Result<()> {
        let exposed = Path::new(HOST_SHARES_DIRECTORY).join(&share.name);
        let raw = raw_root.join(&share.name);
        unmount_if_mounted(&self.umount_program, &exposed)?;
        unmount_if_mounted(&self.umount_program, &raw)?;
        ensure_directory(&exposed, 0o755)?;
        ensure_directory(&raw, 0o755)?;

        let transport = match run(mount_program, &virtiofs_arguments(share, &raw)) {
            Ok(()) => "virtiofs",
            Err(virtiofs_error) => {
                debug!(
                    share = %share.name,
                    %virtiofs_error,
                    "virtiofs host-share mount unavailable; trying virtio-9p"
                );
                run(mount_program, &virtio9p_arguments(share, &raw)).with_context(|| {
                    format!(
                        "failed to mount host share {:?} with virtio-9p after virtiofs failed: {virtiofs_error:#}",
                        share.name
                    )
                })?;
                "virtio-9p"
            }
        };
        if let Err(error) = run(bindfs_program, &bindfs_arguments(share, &raw, &exposed)) {
            let cleanup = unmount_if_mounted(&self.umount_program, &raw);
            return match cleanup {
                Ok(()) => Err(error).with_context(|| {
                    format!(
                        "failed to create idmap-capable view for {transport} host share {:?}",
                        share.name
                    )
                }),
                Err(cleanup) => Err(anyhow!(
                    "failed to create idmap-capable view for {transport} host share {:?}: {error:#}; cleanup failed: {cleanup:#}",
                    share.name
                )),
            };
        }
        debug!(
            share = %share.name,
            path = %exposed.display(),
            writable = share.writable,
            transport,
            "mounted ownership-normalized host share"
        );
        self.mounts.push(MountedHostShare { exposed, raw });
        Ok(())
    }

    fn unmount_best_effort(&mut self) {
        if let Err(error) = self.unmount() {
            warn!(%error, "could not roll back workspace host-share mounts cleanly");
        }
    }
}

impl Drop for HostShareMounts {
    fn drop(&mut self) {
        self.unmount_best_effort();
    }
}

/// Failure to validate, mount, or unmount host directories in the guest.
#[derive(Debug, Error)]
pub(crate) enum HostShareMountError {
    /// The host sent a manifest outside the protocol contract.
    #[error("workspace host-share manifest is invalid")]
    InvalidManifest,
    /// A declared share could not be mounted completely.
    #[error("failed to mount workspace host shares")]
    Mount,
    /// One or more mounted layers could not be unmounted.
    #[error("failed to unmount workspace host shares")]
    Unmount,
}

/// Stable guest root containing the shares attached by the host.
pub(crate) const HOST_SHARES_DIRECTORY: &str = "/mnt/shares";

#[derive(Debug)]
struct MountedHostShare {
    exposed: PathBuf,
    raw: PathBuf,
}

fn mount_error(error: &anyhow::Error) -> Report<HostShareMountError> {
    HostShareMountError::Mount
        .report()
        .message(error.to_string())
}

fn unmount_error(error: &anyhow::Error) -> Report<HostShareMountError> {
    HostShareMountError::Unmount
        .report()
        .message(error.to_string())
}

fn virtiofs_arguments(share: &WorkspaceHostShare, target: &Path) -> Vec<OsString> {
    vec![
        "-t".into(),
        "virtiofs".into(),
        "-o".into(),
        mount_options(share, "nodev,nosuid"),
        "--".into(),
        share.mount_tag.as_str().into(),
        target.as_os_str().to_owned(),
    ]
}

fn virtio9p_arguments(share: &WorkspaceHostShare, target: &Path) -> Vec<OsString> {
    vec![
        "-t".into(),
        "9p".into(),
        "-o".into(),
        mount_options(
            share,
            "trans=virtio,version=9p2000.L,access=any,nodev,nosuid",
        ),
        "--".into(),
        share.mount_tag.as_str().into(),
        target.as_os_str().to_owned(),
    ]
}

fn bindfs_arguments(share: &WorkspaceHostShare, source: &Path, target: &Path) -> Vec<OsString> {
    let mut arguments = vec![
        OsString::from("--force-user=0"),
        OsString::from("--force-group=0"),
        OsString::from(if share.writable {
            "--perms=a+rwX"
        } else {
            "--perms=a+rX"
        }),
        OsString::from("--create-for-user=0"),
        OsString::from("--create-for-group=0"),
        OsString::from("--chown-ignore"),
        OsString::from("--chgrp-ignore"),
        OsString::from("--chmod-ignore"),
        OsString::from("-o"),
        OsString::from(if share.writable {
            "nodev,nosuid"
        } else {
            "ro,nodev,nosuid"
        }),
    ];
    arguments.push(source.as_os_str().to_owned());
    arguments.push(target.as_os_str().to_owned());
    arguments
}

fn mount_options(share: &WorkspaceHostShare, base: &str) -> OsString {
    if share.writable {
        base.into()
    } else {
        format!("ro,{base}").into()
    }
}

fn ensure_directory(path: &Path, mode: u32) -> Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder
        .recursive(true)
        .mode(mode)
        .create(path)
        .with_context(|| {
            format!(
                "failed to create workspace host-share directory {}",
                path.display()
            )
        })?;
    let metadata = fs::symlink_metadata(path).with_context(|| {
        format!(
            "failed to inspect workspace host-share directory {}",
            path.display()
        )
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(anyhow!(
            "workspace host-share path is not a real directory: {}",
            path.display()
        ));
    }
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).with_context(|| {
        format!(
            "failed to set workspace host-share directory permissions {}",
            path.display()
        )
    })
}

fn unmount_if_mounted(program: &Path, path: &Path) -> Result<()> {
    if !is_mountpoint(path)? {
        return Ok(());
    }
    run(
        program,
        &[OsString::from("--"), path.as_os_str().to_owned()],
    )
    .with_context(|| {
        format!(
            "failed to unmount workspace host-share path {}",
            path.display()
        )
    })
}

fn is_mountpoint(path: &Path) -> Result<bool> {
    let mountinfo = fs::read("/proc/self/mountinfo").context("failed to read guest mount table")?;
    for line in mountinfo.split(|byte| *byte == b'\n') {
        let Some(encoded) = line.split(|byte| *byte == b' ').nth(4) else {
            continue;
        };
        if decode_mountinfo_path(encoded) == path.as_os_str() {
            return Ok(true);
        }
    }
    Ok(false)
}

fn decode_mountinfo_path(encoded: &[u8]) -> OsString {
    use std::os::unix::ffi::OsStringExt as _;

    let mut decoded = Vec::with_capacity(encoded.len());
    let mut index = 0;
    while index < encoded.len() {
        if encoded[index] == b'\\' && index + 3 < encoded.len() {
            let digits = &encoded[index + 1..index + 4];
            if digits.iter().all(|digit| matches!(digit, b'0'..=b'7')) {
                decoded.push((digits[0] - b'0') * 64 + (digits[1] - b'0') * 8 + (digits[2] - b'0'));
                index += 4;
                continue;
            }
        }
        decoded.push(encoded[index]);
        index += 1;
    }
    OsString::from_vec(decoded)
}

fn run(program: &Path, arguments: &[OsString]) -> Result<()> {
    let output = Command::new(program)
        .args(arguments)
        .output()
        .with_context(|| format!("failed to start {}", program.display()))?;
    command_result(program, &output)
}

fn command_result(program: &Path, output: &Output) -> Result<()> {
    if output.status.success() {
        return Ok(());
    }
    Err(anyhow!(
        "{} failed with {}: {}",
        program.display(),
        output.status,
        bounded_output(&output.stderr)
    ))
}

fn bounded_output(output: &[u8]) -> String {
    const LIMIT: usize = 2048;
    String::from_utf8_lossy(
        output
            .get(output.len().saturating_sub(LIMIT)..)
            .unwrap_or(output),
    )
    .trim()
    .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn share(writable: bool) -> WorkspaceHostShare {
        WorkspaceHostShare {
            name: "source".to_owned(),
            mount_tag: "tascarrel-share-0".to_owned(),
            writable,
        }
    }

    /// Verifies read-only policy reaches both shared-directory transports.
    #[test]
    fn read_only_is_the_transport_default() {
        let virtiofs = virtiofs_arguments(&share(false), Path::new("/mnt/shares/source"));
        let virtio9p = virtio9p_arguments(&share(false), Path::new("/run/raw/source"));
        assert!(virtiofs.contains(&OsString::from("ro,nodev,nosuid")));
        assert!(virtio9p.contains(&OsString::from(
            "ro,trans=virtio,version=9p2000.L,access=any,nodev,nosuid"
        )));
    }

    /// Verifies the transport-independent bridge normalizes ownership and
    /// preserves read-only policy while supporting pod idmapping.
    #[test]
    fn bindfs_bridge_normalizes_transport_ownership() {
        let arguments = bindfs_arguments(
            &share(false),
            Path::new("/run/raw/source"),
            Path::new("/mnt/shares/source"),
        );
        for expected in [
            "--force-user=0",
            "--force-group=0",
            "--perms=a+rX",
            "ro,nodev,nosuid",
        ] {
            assert!(arguments.contains(&OsString::from(expected)));
        }
    }

    /// Verifies writable shares omit every read-only transport flag.
    #[test]
    fn writable_policy_reaches_every_mount_layer() {
        let share = share(true);
        assert!(
            !virtiofs_arguments(&share, Path::new("/mnt/shares/source"))
                .iter()
                .any(|argument| argument.to_string_lossy().contains("ro,"))
        );
        assert!(
            bindfs_arguments(
                &share,
                Path::new("/run/raw/source"),
                Path::new("/mnt/shares/source")
            )
            .contains(&OsString::from("--perms=a+rwX"))
        );
    }

    /// Verifies mountinfo escaping cannot disguise a mountpoint.
    #[test]
    fn decodes_mountinfo_paths() {
        assert_eq!(
            decode_mountinfo_path(br"/mnt/a\040b\134c"),
            OsString::from("/mnt/a b\\c")
        );
    }
}
