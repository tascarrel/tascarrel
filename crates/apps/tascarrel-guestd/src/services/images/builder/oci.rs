//! OCI archive, manifest, layer, and image-configuration validation.

use std::collections::BTreeSet;

use super::Component;
use super::Digest;
use super::File;
use super::ID_MAP_SIZE;
use super::ImageBuildError;
use super::ImageBuildLimits;
use super::ImageConfig;
use super::ImageId;
use super::ImageUser;
use super::OCI_IMAGE_CONFIG_MEDIA_TYPE;
use super::OCI_IMAGE_MANIFEST_MEDIA_TYPE;
use super::OCI_LAYER_MEDIA_TYPE;
use super::OCI_NONDISTRIBUTABLE_LAYER_MEDIA_TYPE;
use super::OciImageConfiguration;
use super::OciIndex;
use super::OciLayerDescriptor;
use super::OciManifest;
use super::OsStr;
use super::OsStrExt;
use super::Path;
use super::PathBuf;
use super::READ_BUFFER_SIZE;
use super::Read;
use super::Sha256;
use super::TreePolicy;
use super::UmociConfiguration;
use super::ValidatedOciImage;
use super::fs;
use super::read_bounded_metadata;
use super::real_directory;
use super::real_regular_file;
use super::safe_component;
use super::same_metadata;
use super::validate_tree;

pub(crate) fn validate_oci_archive(
    path: &Path,
    limits: &ImageBuildLimits,
) -> Result<(), ImageBuildError> {
    let metadata = real_regular_file(path, "OCI archive")?;
    if metadata.len() > limits.max_oci_archive_bytes {
        return Err(ImageBuildError::OutputLimit {
            kind: "OCI archive",
            path: path.to_path_buf(),
            limit: "maximum archive bytes",
        });
    }
    let file = File::open(path).map_err(|source| ImageBuildError::Io {
        operation: "open OCI archive",
        path: path.to_path_buf(),
        source,
    })?;
    let mut archive = tar::Archive::new(file);
    let entries = archive.entries().map_err(|source| ImageBuildError::Io {
        operation: "read OCI archive",
        path: path.to_path_buf(),
        source,
    })?;
    let mut seen = BTreeSet::new();
    let mut count = 0_u64;
    let mut bytes = 0_u64;
    for entry in entries {
        let entry = entry.map_err(|source| ImageBuildError::Io {
            operation: "read OCI archive entry",
            path: path.to_path_buf(),
            source,
        })?;
        let raw_path = entry.path_bytes();
        let normalized =
            normalize_archive_path(&raw_path).ok_or_else(|| ImageBuildError::UnsafeOutput {
                kind: "OCI archive",
                path: PathBuf::from(OsStr::from_bytes(&raw_path)),
                reason: "entry path is absolute, empty, or contains traversal",
            })?;
        if normalized.split(|byte| *byte == b'/').count() > limits.max_output_depth {
            return Err(ImageBuildError::OutputLimit {
                kind: "OCI archive",
                path: PathBuf::from(OsStr::from_bytes(&raw_path)),
                limit: "maximum directory depth",
            });
        }
        if !seen.insert(normalized) {
            return Err(ImageBuildError::UnsafeOutput {
                kind: "OCI archive",
                path: PathBuf::from(OsStr::from_bytes(&raw_path)),
                reason: "archive contains a duplicate path",
            });
        }
        let entry_type = entry.header().entry_type();
        if !(entry_type.is_file() || entry_type.is_dir()) {
            return Err(ImageBuildError::UnsafeOutput {
                kind: "OCI archive",
                path: PathBuf::from(OsStr::from_bytes(&raw_path)),
                reason: "links and special archive entries are not accepted",
            });
        }
        count = count
            .checked_add(1)
            .ok_or_else(|| ImageBuildError::OutputLimit {
                kind: "OCI archive",
                path: path.to_path_buf(),
                limit: "maximum entry count",
            })?;
        if count > limits.max_output_entries {
            return Err(ImageBuildError::OutputLimit {
                kind: "OCI archive",
                path: path.to_path_buf(),
                limit: "maximum entry count",
            });
        }
        if entry_type.is_file() {
            bytes =
                bytes
                    .checked_add(entry.size())
                    .ok_or_else(|| ImageBuildError::OutputLimit {
                        kind: "OCI archive",
                        path: path.to_path_buf(),
                        limit: "maximum expanded file bytes",
                    })?;
            if bytes > limits.max_output_bytes {
                return Err(ImageBuildError::OutputLimit {
                    kind: "OCI archive",
                    path: path.to_path_buf(),
                    limit: "maximum expanded file bytes",
                });
            }
        }
    }
    Ok(())
}

pub(crate) fn normalize_archive_path(path: &[u8]) -> Option<Vec<u8>> {
    if path.is_empty() || path.contains(&0) {
        return None;
    }
    let path = Path::new(OsStr::from_bytes(path));
    let mut components = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(value) if safe_component(value) => {
                components.push(value.as_bytes());
            }
            Component::CurDir => {}
            _ => return None,
        }
    }
    if components.is_empty() {
        return None;
    }
    let mut normalized = Vec::new();
    for (index, component) in components.into_iter().enumerate() {
        if index != 0 {
            normalized.push(b'/');
        }
        normalized.extend_from_slice(component);
    }
    Some(normalized)
}

pub(crate) fn validate_oci_layout(
    layout: &Path,
    limits: &ImageBuildLimits,
) -> Result<(), ImageBuildError> {
    validate_tree(
        layout,
        TreePolicy {
            kind: "OCI layout",
            allow_symlinks: false,
            allow_hardlinks: false,
            require_mapped_ownership: false,
        },
        limits,
    )?;
    real_regular_file(&layout.join("oci-layout"), "OCI layout marker")?;
    real_regular_file(&layout.join("index.json"), "OCI index")?;
    real_directory(&layout.join("blobs"), "OCI blobs")?;
    Ok(())
}

pub(crate) fn image_from_oci_layout(
    layout: &Path,
    limits: &ImageBuildLimits,
) -> Result<ValidatedOciImage, ImageBuildError> {
    const REFERENCE_ANNOTATION: &str = "org.opencontainers.image.ref.name";
    const REFERENCE: &str = "tascarrel:latest";

    let index_path = layout.join("index.json");
    let index: OciIndex = serde_json::from_slice(&read_bounded_metadata(&index_path, "OCI index")?)
        .map_err(|_| ImageBuildError::UnsafeOutput {
            kind: "OCI index",
            path: index_path.clone(),
            reason: "document is not a valid OCI index",
        })?;
    let mut tagged = index.manifests.iter().filter(|descriptor| {
        descriptor
            .annotations
            .get(REFERENCE_ANNOTATION)
            .is_some_and(|reference| reference == REFERENCE)
    });
    let descriptor = match (tagged.next(), tagged.next()) {
        (Some(descriptor), None) => descriptor,
        (None, None) if index.manifests.len() == 1 => &index.manifests[0],
        _ => {
            return Err(ImageBuildError::UnsafeOutput {
                kind: "OCI index",
                path: index_path,
                reason: "does not contain exactly one tascarrel image manifest",
            });
        }
    };
    if descriptor.media_type != OCI_IMAGE_MANIFEST_MEDIA_TYPE {
        return Err(ImageBuildError::UnsafeOutput {
            kind: "OCI index",
            path: index_path,
            reason: "image manifest has an unsupported media type",
        });
    }
    let manifest_path = sha256_blob_path(
        layout,
        &descriptor.digest,
        "OCI index",
        &index_path,
        "image manifest has an invalid sha256 digest",
    )?;
    let manifest = read_bounded_metadata(&manifest_path, "OCI image manifest")?;
    if u64::try_from(manifest.len()).unwrap_or(u64::MAX) != descriptor.size {
        return Err(ImageBuildError::UnsafeOutput {
            kind: "OCI image manifest",
            path: manifest_path,
            reason: "content does not match the descriptor size",
        });
    }
    let actual = format!("sha256:{:x}", Sha256::digest(&manifest));
    if actual != descriptor.digest {
        return Err(ImageBuildError::UnsafeOutput {
            kind: "OCI image manifest",
            path: manifest_path.clone(),
            reason: "content does not match the descriptor digest",
        });
    }
    let parsed: OciManifest =
        serde_json::from_slice(&manifest).map_err(|_| ImageBuildError::UnsafeOutput {
            kind: "OCI image manifest",
            path: manifest_path.clone(),
            reason: "document is not a valid OCI image manifest",
        })?;
    let configured_user = configured_user_from_oci_image(layout, &parsed.config, &manifest_path)?;
    validate_oci_layer_ownership(layout, &parsed.layers, limits)?;
    let id =
        ImageId::new(descriptor.digest.clone()).map_err(|_| ImageBuildError::UnsafeOutput {
            kind: "OCI index",
            path: index_path,
            reason: "image manifest has an invalid sha256 digest",
        })?;
    Ok(ValidatedOciImage {
        id,
        configured_user,
    })
}

pub(crate) fn configured_user_from_oci_image(
    layout: &Path,
    descriptor: &OciLayerDescriptor,
    manifest_path: &Path,
) -> Result<String, ImageBuildError> {
    if descriptor.media_type != OCI_IMAGE_CONFIG_MEDIA_TYPE {
        return Err(ImageBuildError::UnsafeOutput {
            kind: "OCI image config",
            path: manifest_path.to_path_buf(),
            reason: "image config has an unsupported media type",
        });
    }
    let config_path = sha256_blob_path(
        layout,
        &descriptor.digest,
        "OCI image manifest",
        manifest_path,
        "image config has an invalid sha256 digest",
    )?;
    let config = read_bounded_metadata(&config_path, "OCI image config")?;
    if u64::try_from(config.len()).unwrap_or(u64::MAX) != descriptor.size {
        return Err(ImageBuildError::UnsafeOutput {
            kind: "OCI image config",
            path: config_path,
            reason: "content does not match the descriptor size",
        });
    }
    let actual = format!("sha256:{:x}", Sha256::digest(&config));
    if actual != descriptor.digest {
        return Err(ImageBuildError::UnsafeOutput {
            kind: "OCI image config",
            path: config_path.clone(),
            reason: "content does not match the descriptor digest",
        });
    }
    let configuration: OciImageConfiguration =
        serde_json::from_slice(&config).map_err(|_| ImageBuildError::UnsafeOutput {
            kind: "OCI image config",
            path: config_path,
            reason: "document is not a valid OCI image configuration",
        })?;
    Ok(configuration
        .config
        .and_then(|config| config.user)
        .unwrap_or_default())
}

pub(crate) fn sha256_blob_path(
    layout: &Path,
    digest: &str,
    kind: &'static str,
    error_path: &Path,
    reason: &'static str,
) -> Result<PathBuf, ImageBuildError> {
    let Some(hexadecimal) = digest.strip_prefix("sha256:") else {
        return Err(ImageBuildError::UnsafeOutput {
            kind,
            path: error_path.to_path_buf(),
            reason,
        });
    };
    if hexadecimal.len() != 64
        || !hexadecimal
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err(ImageBuildError::UnsafeOutput {
            kind,
            path: error_path.to_path_buf(),
            reason,
        });
    }
    Ok(layout.join("blobs").join("sha256").join(hexadecimal))
}

#[derive(Clone, Copy)]
pub(crate) enum LayerCompression {
    None,
    Gzip,
    Zstd,
}

pub(crate) fn validate_oci_layer_ownership(
    layout: &Path,
    layers: &[OciLayerDescriptor],
    limits: &ImageBuildLimits,
) -> Result<(), ImageBuildError> {
    let mut entries = 0_u64;
    let mut bytes = 0_u64;
    for descriptor in layers {
        let blob = sha256_blob_path(
            layout,
            &descriptor.digest,
            "OCI image manifest",
            &layout.join("index.json"),
            "layer has an invalid sha256 digest",
        )?;
        validate_layer_blob(&blob, descriptor)?;
        let file = File::open(&blob).map_err(|source| ImageBuildError::Io {
            operation: "open OCI image layer",
            path: blob.clone(),
            source,
        })?;
        let reader: Box<dyn Read> = match layer_compression(&descriptor.media_type) {
            Some(LayerCompression::None) => Box::new(file),
            // OCI gzip layers may contain multiple RFC 1952 members. Reading
            // only the first would validate a different tar stream from Go's
            // archive stack (and therefore umoci).
            Some(LayerCompression::Gzip) => Box::new(flate2::read::MultiGzDecoder::new(file)),
            Some(LayerCompression::Zstd) => {
                Box::new(zstd::stream::read::Decoder::new(file).map_err(|source| {
                    ImageBuildError::Io {
                        operation: "decompress OCI image layer",
                        path: blob.clone(),
                        source,
                    }
                })?)
            }
            None => {
                return Err(ImageBuildError::UnsafeOutput {
                    kind: "OCI image layer",
                    path: blob,
                    reason: "layer has an unsupported media type",
                });
            }
        };
        validate_layer_tar(reader, &blob, limits, &mut entries, &mut bytes)?;
    }
    Ok(())
}

pub(crate) fn layer_compression(media_type: &str) -> Option<LayerCompression> {
    for base in [OCI_LAYER_MEDIA_TYPE, OCI_NONDISTRIBUTABLE_LAYER_MEDIA_TYPE] {
        if media_type == base {
            return Some(LayerCompression::None);
        }
        if media_type == format!("{base}+gzip") {
            return Some(LayerCompression::Gzip);
        }
        if media_type == format!("{base}+zstd") {
            return Some(LayerCompression::Zstd);
        }
    }
    None
}

pub(crate) fn validate_layer_blob(
    path: &Path,
    descriptor: &OciLayerDescriptor,
) -> Result<(), ImageBuildError> {
    let expected = fs::symlink_metadata(path).map_err(|source| ImageBuildError::Io {
        operation: "inspect OCI image layer",
        path: path.to_path_buf(),
        source,
    })?;
    if expected.file_type().is_symlink() || !expected.is_file() || expected.len() != descriptor.size
    {
        return Err(ImageBuildError::UnsafeOutput {
            kind: "OCI image layer",
            path: path.to_path_buf(),
            reason: "blob is not a real file of the descriptor size",
        });
    }
    let mut file = File::open(path).map_err(|source| ImageBuildError::Io {
        operation: "open OCI image layer",
        path: path.to_path_buf(),
        source,
    })?;
    let mut hasher = Sha256::new();
    let mut read = 0_u64;
    let mut buffer = vec![0_u8; READ_BUFFER_SIZE].into_boxed_slice();
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|source| ImageBuildError::Io {
                operation: "hash OCI image layer",
                path: path.to_path_buf(),
                source,
            })?;
        if count == 0 {
            break;
        }
        read = read
            .checked_add(u64::try_from(count).unwrap_or(u64::MAX))
            .ok_or_else(|| ImageBuildError::UnsafeOutput {
                kind: "OCI image layer",
                path: path.to_path_buf(),
                reason: "blob grew while it was being hashed",
            })?;
        if read > expected.len() {
            return Err(ImageBuildError::UnsafeOutput {
                kind: "OCI image layer",
                path: path.to_path_buf(),
                reason: "blob grew while it was being hashed",
            });
        }
        hasher.update(&buffer[..count]);
    }
    let after = file.metadata().map_err(|source| ImageBuildError::Io {
        operation: "reinspect OCI image layer",
        path: path.to_path_buf(),
        source,
    })?;
    if read != expected.len() || !same_metadata(&expected, &after) {
        return Err(ImageBuildError::UnsafeOutput {
            kind: "OCI image layer",
            path: path.to_path_buf(),
            reason: "blob changed while it was being hashed",
        });
    }
    let actual = format!("sha256:{:x}", hasher.finalize());
    if actual != descriptor.digest {
        return Err(ImageBuildError::UnsafeOutput {
            kind: "OCI image layer",
            path: path.to_path_buf(),
            reason: "content does not match the descriptor digest",
        });
    }
    Ok(())
}

pub(crate) fn validate_layer_tar(
    reader: Box<dyn Read>,
    path: &Path,
    limits: &ImageBuildLimits,
    entries: &mut u64,
    bytes: &mut u64,
) -> Result<(), ImageBuildError> {
    let mut archive = tar::Archive::new(reader);
    let archive_entries = archive.entries().map_err(|source| ImageBuildError::Io {
        operation: "read OCI image layer",
        path: path.to_path_buf(),
        source,
    })?;
    for entry in archive_entries {
        let mut entry = entry.map_err(|source| ImageBuildError::Io {
            operation: "read OCI image layer entry",
            path: path.to_path_buf(),
            source,
        })?;
        let entry_type = entry.header().entry_type();
        if entry_type.is_pax_global_extensions() {
            return Err(ImageBuildError::UnsafeOutput {
                kind: "OCI image layer",
                path: path.to_path_buf(),
                reason: "global PAX headers are not supported by the image unpacker",
            });
        }
        validate_layer_pax_ownership(&mut entry, path)?;
        let uid = entry
            .header()
            .uid()
            .map_err(|_| ImageBuildError::UnsafeOutput {
                kind: "OCI image layer",
                path: path.to_path_buf(),
                reason: "entry has an invalid UID",
            })?;
        let gid = entry
            .header()
            .gid()
            .map_err(|_| ImageBuildError::UnsafeOutput {
                kind: "OCI image layer",
                path: path.to_path_buf(),
                reason: "entry has an invalid GID",
            })?;
        if uid >= u64::from(ID_MAP_SIZE) || gid >= u64::from(ID_MAP_SIZE) {
            return Err(ImageBuildError::UnsafeOutput {
                kind: "OCI image layer",
                path: path.to_path_buf(),
                reason: "entry owner is outside the pod user-namespace map",
            });
        }
        *entries = entries
            .checked_add(1)
            .ok_or_else(|| ImageBuildError::OutputLimit {
                kind: "OCI image layers",
                path: path.to_path_buf(),
                limit: "maximum entry count",
            })?;
        if *entries > limits.max_output_entries {
            return Err(ImageBuildError::OutputLimit {
                kind: "OCI image layers",
                path: path.to_path_buf(),
                limit: "maximum entry count",
            });
        }
        if entry_type.is_file() {
            *bytes =
                bytes
                    .checked_add(entry.size())
                    .ok_or_else(|| ImageBuildError::OutputLimit {
                        kind: "OCI image layers",
                        path: path.to_path_buf(),
                        limit: "maximum aggregate file bytes",
                    })?;
            if *bytes > limits.max_output_bytes {
                return Err(ImageBuildError::OutputLimit {
                    kind: "OCI image layers",
                    path: path.to_path_buf(),
                    limit: "maximum aggregate file bytes",
                });
            }
        }
    }
    Ok(())
}

pub(crate) fn validate_layer_pax_ownership<R: Read>(
    entry: &mut tar::Entry<'_, R>,
    path: &Path,
) -> Result<(), ImageBuildError> {
    let Some(extensions) = entry
        .pax_extensions()
        .map_err(|_| ImageBuildError::UnsafeOutput {
            kind: "OCI image layer",
            path: path.to_path_buf(),
            reason: "entry has malformed PAX metadata",
        })?
    else {
        return Ok(());
    };
    let mut uid_seen = false;
    let mut gid_seen = false;
    for extension in extensions {
        let extension = extension.map_err(|_| ImageBuildError::UnsafeOutput {
            kind: "OCI image layer",
            path: path.to_path_buf(),
            reason: "entry has malformed PAX metadata",
        })?;
        let seen = match extension.key_bytes() {
            b"uid" => &mut uid_seen,
            b"gid" => &mut gid_seen,
            _ => continue,
        };
        if *seen {
            return Err(ImageBuildError::UnsafeOutput {
                kind: "OCI image layer",
                path: path.to_path_buf(),
                reason: "entry has duplicate PAX ownership metadata",
            });
        }
        *seen = true;
        let value = extension.value_bytes();
        // Both Rust's tar reader and Go's archive/tar retain the classic
        // header value for an empty local PAX value. The effective header is
        // still checked immediately after this function returns.
        if value.is_empty() {
            continue;
        }
        let parsed = std::str::from_utf8(value)
            .ok()
            .and_then(|value| value.parse::<u64>().ok());
        if parsed.is_none_or(|id| id >= u64::from(ID_MAP_SIZE)) {
            return Err(ImageBuildError::UnsafeOutput {
                kind: "OCI image layer",
                path: path.to_path_buf(),
                reason: "PAX entry owner is outside the pod user-namespace map",
            });
        }
    }
    Ok(())
}

pub(crate) fn image_config_from_umoci_bundle(
    bundle: &Path,
    configured_user: &str,
) -> Result<ImageConfig, ImageBuildError> {
    let config_path = bundle.join("config.json");
    let configuration: UmociConfiguration =
        serde_json::from_slice(&read_bounded_metadata(&config_path, "umoci config")?).map_err(
            |_| ImageBuildError::UnsafeOutput {
                kind: "umoci config",
                path: config_path,
                reason: "document is not valid JSON",
            },
        )?;
    let process = configuration.process;
    let name = configured_user
        .split_once(':')
        .map_or(configured_user, |(user, _)| user);
    let name = if name.is_empty() {
        if process.user.uid == 0 {
            "root".to_owned()
        } else {
            process.user.uid.to_string()
        }
    } else {
        name.to_owned()
    };
    let user = ImageUser::new(
        name,
        process.user.uid,
        process.user.gid,
        process.user.additional_gids,
    )
    .map_err(|error| ImageBuildError::InvalidImageConfig(error.to_string()))?;
    ImageConfig::for_process(process.env, user, process.cwd)
        .map_err(|error| ImageBuildError::InvalidImageConfig(error.to_string()))
}
