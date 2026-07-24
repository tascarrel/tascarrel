//! Streaming protocols for browser-supplied chat attachments.

use serde::Deserialize;
use serde::Serialize;
use tascarrel_api::ids::ChatAttachmentId;
use tascarrel_api::types::chats::ChatPromptAttachment;

/// Maximum number of bytes accepted for one browser-supplied chat attachment.
pub const MAX_CHAT_ATTACHMENT_BYTES: u64 = 25 * 1024 * 1024;
/// Host-to-guest stream carrying one browser-supplied chat attachment.
pub const MUX_CHAT_ATTACHMENT_UPLOAD_ENDPOINT: &str = "tascarrel-chat-attachment-upload-v1";
/// Host-to-guest request followed by a guest-to-host chat attachment stream.
pub const MUX_CHAT_ATTACHMENT_READ_ENDPOINT: &str = "tascarrel-chat-attachment-read-v1";

/// Metadata sent before the raw attachment bytes.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ChatAttachmentUploadRequest {
    /// User-facing filename.
    pub name: String,
    /// Declared MIME media type without parameters.
    pub media_type: String,
}

/// Result returned after the attachment body has been consumed.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "status", rename_all = "camelCase")]
pub enum ChatAttachmentUploadResponse {
    /// The attachment was stored and can be referenced from a chat prompt.
    Uploaded {
        /// Path-free attachment metadata returned to the browser.
        attachment: ChatPromptAttachment,
    },
    /// The attachment was rejected by the guest chat engine.
    Rejected {
        /// Stable chat-engine failure category.
        code: String,
        /// Human-readable diagnostic safe to expose to the caller.
        message: String,
    },
}

/// Requests the immutable bytes for one stored chat attachment.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ChatAttachmentReadRequest {
    /// Attachment to read from workspace chat storage.
    pub attachment_id: ChatAttachmentId,
}

/// Metadata sent before a successful raw attachment byte stream.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "status", rename_all = "camelCase")]
pub enum ChatAttachmentReadResponse {
    /// Raw attachment bytes follow this frame until channel EOF.
    Found {
        /// Path-free metadata describing the following bytes.
        attachment: ChatPromptAttachment,
    },
    /// The requested attachment could not be read.
    Rejected {
        /// Stable chat-engine failure category.
        code: String,
        /// Human-readable diagnostic safe to expose to the caller.
        message: String,
    },
}
