//! Managed input attachments for chat prompts.

use std::fmt;
use std::fs;
use std::io;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::path::PathBuf;

use nix::unistd::Gid;
use nix::unistd::Uid;
use sha2::Digest as _;
use sha2::Sha256;
use tascarrel_api::ids::ChatAttachmentId;
use tascarrel_api::ids::ChatId;
use tascarrel_api::types::chats::ChatPromptAttachment;
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt as _;
use tokio::io::AsyncWriteExt as _;

const MAX_ATTACHMENT_NAME_BYTES: usize = 255;
const MAX_MEDIA_TYPE_BYTES: usize = 128;
const MAX_METADATA_BYTES: u64 = 16 * 1024;

/// Metadata supplied when storing a new chat attachment.
#[derive(Clone, Debug, PartialEq)]
pub struct StoreChatAttachmentRequest {
    /// User-facing filename.
    pub name: String,
    /// Declared MIME media type.
    pub media_type: String,
}

#[derive(Clone, Debug)]
pub(crate) struct AttachmentStore {
    root: PathBuf,
    max_attachment_bytes: u64,
    owner: Option<(u32, u32)>,
    binding_root: Option<PathBuf>,
}

#[derive(Clone, Debug)]
pub(crate) struct ResolvedChatAttachment {
    pub attachment: ChatPromptAttachment,
    pub source_path: PathBuf,
    pub path: PathBuf,
}

#[derive(serde::Deserialize, serde::Serialize)]
struct StoredAttachmentMetadata {
    attachment: ChatPromptAttachment,
}

impl AttachmentStore {
    pub fn new(
        root: PathBuf,
        max_attachment_bytes: u64,
        owner: Option<(u32, u32)>,
        binding_root: Option<PathBuf>,
    ) -> Self {
        Self {
            root,
            max_attachment_bytes,
            owner,
            binding_root,
        }
    }

    pub async fn store<R>(
        &self,
        request: StoreChatAttachmentRequest,
        mut reader: R,
    ) -> Result<ChatPromptAttachment, AttachmentStoreError>
    where
        R: AsyncRead + Unpin,
    {
        validate_name(&request.name)?;
        validate_media_type(&request.media_type)?;
        if self.max_attachment_bytes == 0 {
            return Err(AttachmentStoreError::InvalidInput(
                "the attachment size limit must be greater than zero".to_owned(),
            ));
        }

        let staging = self.root.join("staging");
        let attachments = self.root.join("attachments");
        create_private_directory(&staging).await?;
        create_private_directory(&attachments).await?;

        let attachment_id = ChatAttachmentId::generate();
        let temporary_path = staging.join(format!("{}.part", attachment_id.0));
        let mut temporary = tokio::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary_path)
            .await?;
        set_private_file_permissions(&temporary_path).await?;

        let write_result =
            async {
                let mut hasher = Sha256::new();
                let mut size = 0_u64;
                let mut buffer = vec![0_u8; 64 * 1024].into_boxed_slice();
                loop {
                    let read = reader.read(&mut buffer).await?;
                    if read == 0 {
                        break;
                    }
                    size = size
                        .checked_add(u64::try_from(read).map_err(|_| {
                            AttachmentStoreError::TooLarge(self.max_attachment_bytes)
                        })?)
                        .ok_or(AttachmentStoreError::TooLarge(self.max_attachment_bytes))?;
                    if size > self.max_attachment_bytes {
                        return Err(AttachmentStoreError::TooLarge(self.max_attachment_bytes));
                    }
                    hasher.update(&buffer[..read]);
                    temporary.write_all(&buffer[..read]).await?;
                }
                temporary.flush().await?;
                temporary.sync_all().await?;
                Ok::<_, AttachmentStoreError>((size, hex_digest(hasher.finalize().as_slice())))
            }
            .await;
        drop(temporary);
        let (size, digest) = match write_result {
            Ok(result) => result,
            Err(error) => {
                if let Err(cleanup_error) = tokio::fs::remove_file(&temporary_path).await {
                    tracing::warn!(
                        path = %temporary_path.display(),
                        %cleanup_error,
                        "failed to remove an incomplete chat attachment"
                    );
                }
                return Err(error);
            }
        };

        let attachment = ChatPromptAttachment {
            attachment_id: attachment_id.clone(),
            name: request.name.into(),
            media_type: request.media_type.into(),
            size,
            digest: digest.into(),
        };
        let metadata = StoredAttachmentMetadata {
            attachment: attachment.clone(),
        };
        let attachment_directory = attachments.join(attachment_id.0.as_ref());
        tokio::fs::create_dir(&attachment_directory).await?;
        set_private_directory_permissions(&attachment_directory).await?;
        let content_path = attachment_directory.join("content");
        if let Err(error) = tokio::fs::rename(&temporary_path, &content_path).await {
            if let Err(cleanup_error) = tokio::fs::remove_dir(&attachment_directory).await {
                tracing::warn!(
                    path = %attachment_directory.display(),
                    %cleanup_error,
                    "failed to remove an empty chat attachment directory"
                );
            }
            return Err(error.into());
        }
        if let Err(error) = set_private_read_only_file_permissions(&content_path).await {
            if let Err(cleanup_error) = tokio::fs::remove_dir_all(&attachment_directory).await {
                tracing::warn!(
                    path = %attachment_directory.display(),
                    %cleanup_error,
                    "failed to remove a rejected chat attachment"
                );
            }
            return Err(error);
        }

        let metadata_result = write_metadata(&attachment_directory, &metadata).await;
        if let Err(error) = metadata_result {
            if let Err(cleanup_error) = tokio::fs::remove_dir_all(&attachment_directory).await {
                tracing::warn!(
                    path = %attachment_directory.display(),
                    %cleanup_error,
                    "failed to remove a chat attachment with incomplete metadata"
                );
            }
            return Err(error);
        }
        if let Some((uid, gid)) = self.owner
            && let Err(error) = prepare_binding_tree(&attachment_directory, uid, gid)
        {
            if let Err(cleanup_error) = tokio::fs::remove_dir_all(&attachment_directory).await {
                tracing::warn!(
                    path = %attachment_directory.display(),
                    %cleanup_error,
                    "failed to remove a chat attachment with invalid ownership"
                );
            }
            return Err(error);
        }
        Ok(attachment)
    }

    pub async fn resolve(
        &self,
        attachment_id: &ChatAttachmentId,
    ) -> Result<ResolvedChatAttachment, AttachmentStoreError> {
        let attachment_directory = self.root.join("attachments").join(attachment_id.0.as_ref());
        let metadata_path = attachment_directory.join("metadata.json");
        let metadata_file = tokio::fs::metadata(&metadata_path)
            .await
            .map_err(map_not_found)?;
        if !metadata_file.is_file() || metadata_file.len() > MAX_METADATA_BYTES {
            return Err(AttachmentStoreError::Corrupt(
                "attachment metadata is not a bounded regular file".to_owned(),
            ));
        }
        let encoded = tokio::fs::read(&metadata_path)
            .await
            .map_err(map_not_found)?;
        let metadata: StoredAttachmentMetadata =
            serde_json::from_slice(&encoded).map_err(|error| {
                AttachmentStoreError::Corrupt(format!(
                    "unable to decode attachment metadata: {error}"
                ))
            })?;
        if &metadata.attachment.attachment_id != attachment_id {
            return Err(AttachmentStoreError::Corrupt(
                "attachment metadata contains a different identifier".to_owned(),
            ));
        }

        let content_path = attachment_directory.join("content");
        let content = tokio::fs::metadata(&content_path)
            .await
            .map_err(map_not_found)?;
        if !content.is_file() || content.len() != metadata.attachment.size {
            return Err(AttachmentStoreError::Corrupt(
                "attachment content does not match its metadata".to_owned(),
            ));
        }
        let canonical_root = tokio::fs::canonicalize(&self.root).await?;
        let canonical_path = tokio::fs::canonicalize(content_path).await?;
        if !canonical_path.starts_with(&canonical_root) {
            return Err(AttachmentStoreError::Corrupt(
                "attachment content escapes the configured store".to_owned(),
            ));
        }
        let path = if let Some(binding_root) = &self.binding_root {
            let canonical_attachments = tokio::fs::canonicalize(self.root.join("attachments"))
                .await
                .map_err(map_not_found)?;
            let relative = canonical_path
                .strip_prefix(&canonical_attachments)
                .map_err(|_| {
                    AttachmentStoreError::Corrupt(
                        "attachment content escapes the binding-visible store".to_owned(),
                    )
                })?;
            binding_root.join(relative)
        } else {
            canonical_path.clone()
        };
        Ok(ResolvedChatAttachment {
            attachment: metadata.attachment,
            source_path: canonical_path,
            path,
        })
    }

    pub async fn associate_with_chat(
        &self,
        chat_id: &ChatId,
        attachment_id: &ChatAttachmentId,
    ) -> Result<(), AttachmentStoreError> {
        let chats = self.root.join("chats");
        let chat_directory = chats.join(chat_id.0.as_ref());
        create_private_directory(&chats).await?;
        create_private_directory(&chat_directory).await?;
        let marker_path = chat_directory.join(attachment_id.0.as_ref());
        match tokio::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&marker_path)
            .await
        {
            Ok(marker) => {
                drop(marker);
                set_private_file_permissions(&marker_path).await?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
        Ok(())
    }
}

/// Creates or repairs the immutable attachment tree exposed through the
/// pod-user idmapped mount.
pub(crate) fn prepare_binding_tree(
    root: &Path,
    uid: u32,
    gid: u32,
) -> Result<(), AttachmentStoreError> {
    fs::create_dir_all(root)?;
    prepare_binding_entry(root, uid, gid)
}

fn prepare_binding_entry(path: &Path, uid: u32, gid: u32) -> Result<(), AttachmentStoreError> {
    let metadata = fs::symlink_metadata(path)?;
    let file_type = metadata.file_type();
    if !file_type.is_dir() && !file_type.is_file() {
        return Err(AttachmentStoreError::Corrupt(format!(
            "attachment tree contains an unsupported entry: {}",
            path.display(),
        )));
    }
    nix::unistd::chown(path, Some(Uid::from_raw(uid)), Some(Gid::from_raw(gid)))
        .map_err(|error| io::Error::from_raw_os_error(error as i32))?;
    #[cfg(unix)]
    fs::set_permissions(
        path,
        fs::Permissions::from_mode(if file_type.is_dir() { 0o700 } else { 0o400 }),
    )?;
    if file_type.is_dir() {
        for entry in fs::read_dir(path)? {
            prepare_binding_entry(&entry?.path(), uid, gid)?;
        }
    }
    Ok(())
}

async fn write_metadata(
    attachment_directory: &Path,
    metadata: &StoredAttachmentMetadata,
) -> Result<(), AttachmentStoreError> {
    let encoded = serde_json::to_vec(metadata).map_err(|error| {
        AttachmentStoreError::Corrupt(format!("unable to encode attachment metadata: {error}"))
    })?;
    let temporary_path = attachment_directory.join("metadata.json.part");
    let final_path = attachment_directory.join("metadata.json");
    let mut file = tokio::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary_path)
        .await?;
    set_private_file_permissions(&temporary_path).await?;
    file.write_all(&encoded).await?;
    file.flush().await?;
    file.sync_all().await?;
    drop(file);
    tokio::fs::rename(temporary_path, final_path).await?;
    Ok(())
}

fn validate_name(name: &str) -> Result<(), AttachmentStoreError> {
    if name.is_empty()
        || name.len() > MAX_ATTACHMENT_NAME_BYTES
        || name == "."
        || name == ".."
        || name.contains(['/', '\\'])
        || name.chars().any(char::is_control)
    {
        return Err(AttachmentStoreError::InvalidInput(
            "attachment name is invalid".to_owned(),
        ));
    }
    Ok(())
}

fn validate_media_type(media_type: &str) -> Result<(), AttachmentStoreError> {
    if media_type.is_empty()
        || media_type.len() > MAX_MEDIA_TYPE_BYTES
        || !media_type
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'+' | b'-' | b'.'))
        || !media_type.contains('/')
    {
        return Err(AttachmentStoreError::InvalidInput(
            "attachment media type is invalid".to_owned(),
        ));
    }
    Ok(())
}

fn hex_digest(digest: &[u8]) -> String {
    use std::fmt::Write as _;

    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        write!(&mut encoded, "{byte:02x}").expect("writing to a string cannot fail");
    }
    encoded
}

async fn create_private_directory(path: &Path) -> Result<(), AttachmentStoreError> {
    tokio::fs::create_dir_all(path).await?;
    set_private_directory_permissions(path).await
}

#[cfg(unix)]
async fn set_private_directory_permissions(path: &Path) -> Result<(), AttachmentStoreError> {
    use std::os::unix::fs::PermissionsExt as _;

    tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).await?;
    Ok(())
}

#[cfg(not(unix))]
async fn set_private_directory_permissions(_path: &Path) -> Result<(), AttachmentStoreError> {
    Ok(())
}

#[cfg(unix)]
async fn set_private_file_permissions(path: &Path) -> Result<(), AttachmentStoreError> {
    use std::os::unix::fs::PermissionsExt as _;

    tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).await?;
    Ok(())
}

#[cfg(not(unix))]
async fn set_private_file_permissions(_path: &Path) -> Result<(), AttachmentStoreError> {
    Ok(())
}

#[cfg(unix)]
async fn set_private_read_only_file_permissions(path: &Path) -> Result<(), AttachmentStoreError> {
    use std::os::unix::fs::PermissionsExt as _;

    tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o400)).await?;
    Ok(())
}

#[cfg(not(unix))]
async fn set_private_read_only_file_permissions(path: &Path) -> Result<(), AttachmentStoreError> {
    let mut permissions = tokio::fs::metadata(path).await?.permissions();
    permissions.set_readonly(true);
    tokio::fs::set_permissions(path, permissions).await?;
    Ok(())
}

fn map_not_found(error: std::io::Error) -> AttachmentStoreError {
    if error.kind() == std::io::ErrorKind::NotFound {
        AttachmentStoreError::NotFound
    } else {
        error.into()
    }
}

#[derive(Debug)]
pub(crate) enum AttachmentStoreError {
    InvalidInput(String),
    TooLarge(u64),
    NotFound,
    Corrupt(String),
    Io(std::io::Error),
}

impl fmt::Display for AttachmentStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidInput(message) | Self::Corrupt(message) => formatter.write_str(message),
            Self::TooLarge(limit) => write!(formatter, "attachment exceeds the {limit}-byte limit"),
            Self::NotFound => formatter.write_str("attachment does not exist"),
            Self::Io(error) => write!(formatter, "attachment storage failed: {error}"),
        }
    }
}

impl std::error::Error for AttachmentStoreError {}

impl From<std::io::Error> for AttachmentStoreError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Stores content while exposing distinct guest and pod paths.
    #[tokio::test]
    async fn stored_attachment_resolves_to_separate_guest_and_pod_paths() {
        let temporary = tempfile::tempdir().unwrap();
        let store = AttachmentStore::new(
            temporary.path().join("store"),
            1024,
            Some((Uid::effective().as_raw(), Gid::effective().as_raw())),
            Some(PathBuf::from("/opt/tascarrel/chat/attachments")),
        );
        let attachment = store
            .store(
                StoreChatAttachmentRequest {
                    name: "notes.md".to_owned(),
                    media_type: "text/markdown".to_owned(),
                },
                &b"# Uploaded\n"[..],
            )
            .await
            .unwrap();
        let resolved = store.resolve(&attachment.attachment_id).await.unwrap();

        assert_eq!(
            tokio::fs::read(&resolved.source_path).await.unwrap(),
            b"# Uploaded\n"
        );
        assert_eq!(
            resolved.path,
            PathBuf::from("/opt/tascarrel/chat/attachments")
                .join(attachment.attachment_id.0.as_ref())
                .join("content"),
        );
    }
}
