//! Durable coding-harness chats owned by the workspace guest daemon.
//!
//! The feature separates durable reducer state from runtime harness bindings.
//! Harness-specific adaptors translate native protocols into the normalized
//! event model consumed by the state layer.

use std::path::PathBuf;
use std::sync::Arc;

use reportify::Report;
use tascarrel_api::ids::ChatAttachmentId;
use tascarrel_api::types::chats::ChatPromptAttachment;
use tascarrel_api::types::pods::PodId;
use thiserror::Error;
use tokio::io::AsyncRead;

use crate::ChatStorage;
use crate::Database;

mod adaptors;
mod attachment;
mod auth;
mod binding;
mod engine;
mod harness;
mod manager;
mod pricing;
mod process;
mod state;
mod title;

use engine::ChatEngine;
pub use engine::ChatEngineError;
use engine::ChatEngineOptions;
pub(crate) use manager::HarnessListSubscription;
use manager::HarnessManager;
pub(crate) use manager::HarnessManagerError;
pub(crate) use state::ChatListStoreSubscription;
use state::ChatState;
pub(crate) use state::ChatStoreSubscription;
pub(crate) use state::UsageReportSubscription;
pub(crate) use title::fallback_title;

/// Workspace chat feature composed from durable state and harness management.
#[derive(Clone)]
pub struct ChatService {
    engine: Arc<ChatEngine>,
    harnesses: Arc<HarnessManager>,
}

impl ChatService {
    /// Opens chat state in the guest database and prepares workspace harness
    /// state.
    ///
    /// # Errors
    ///
    /// Returns an error when durable state or workspace harness state cannot
    /// be opened.
    pub async fn open(
        database: Database,
        storage: &ChatStorage,
        harness_user_id: u32,
        harness_group_id: u32,
        tasci_executable: PathBuf,
    ) -> Result<Self, Report<ChatServiceError>> {
        let attachment_store_path = storage.root().join("attachments");
        attachment::prepare_binding_tree(
            &attachment_store_path.join("attachments"),
            harness_user_id,
            harness_group_id,
        )
        .map_err(|error| Report::new(ChatServiceError::Attachment(error.to_string())))?;
        let harnesses = HarnessManager::open(
            storage.root().to_owned(),
            harness_user_id,
            harness_group_id,
            tasci_executable,
        )
        .map_err(|report| report.escalate(ChatServiceError::Harnesses))?;
        let state = ChatState::new(database.connection().clone())
            .await
            .map_err(|error| Report::new(ChatServiceError::State(error.message)))?;
        let provider = Arc::clone(&harnesses) as Arc<dyn binding::BindingProvider>;
        let options = ChatEngineOptions {
            attachment_store_path: Some(attachment_store_path),
            attachment_binding_path: Some(PathBuf::from("/opt/tascarrel/chat/attachments")),
            attachment_owner: Some((harness_user_id, harness_group_id)),
            ..ChatEngineOptions::default()
        };
        let title_generator = Arc::clone(&harnesses) as Arc<dyn title::TitleGenerationService>;
        let engine = Arc::new(ChatEngine::with_options_and_title_generator(
            Arc::new(state),
            provider,
            options,
            Some(title_generator),
        ));
        Ok(Self { engine, harnesses })
    }

    /// Starts pricing refresh and retries installation of every pinned harness.
    pub fn start_eager_installation(&self) {
        self.harnesses.start_eager_installation();
    }

    /// Returns the durable binding-aware chat engine.
    pub(crate) fn engine(&self) -> &ChatEngine {
        self.engine.as_ref()
    }

    /// Archives every active chat owned by one pod.
    pub(crate) async fn archive_pod_chats(&self, pod_id: &PodId) -> Result<(), ChatEngineError> {
        self.engine.archive_pod_chats(pod_id).await
    }

    /// Streams a browser-supplied prompt attachment into workspace chat state.
    ///
    /// # Errors
    ///
    /// Returns a client-safe chat-engine error when the metadata or content is
    /// invalid, storage fails, or the chat engine is shutting down.
    pub async fn store_attachment<R>(
        &self,
        name: String,
        media_type: String,
        reader: R,
    ) -> Result<ChatPromptAttachment, ChatEngineError>
    where
        R: AsyncRead + Unpin,
    {
        self.engine
            .store_chat_attachment(
                attachment::StoreChatAttachmentRequest { name, media_type },
                reader,
            )
            .await
    }

    /// Opens one immutable attachment for the guest transport service.
    ///
    /// # Errors
    ///
    /// Returns a client-safe chat-engine error when the attachment cannot be
    /// resolved or opened.
    pub async fn open_attachment(
        &self,
        attachment_id: &ChatAttachmentId,
    ) -> Result<(ChatPromptAttachment, tokio::fs::File), ChatEngineError> {
        self.engine.open_chat_attachment(attachment_id).await
    }

    /// Returns workspace harness installation and authentication state.
    pub(crate) fn harnesses(&self) -> &Arc<HarnessManager> {
        &self.harnesses
    }

    /// Stops runtime bindings and checkpoints durable chat state.
    ///
    /// # Errors
    ///
    /// Returns an error when a binding cannot be stopped or chat state cannot
    /// be checkpointed.
    pub async fn shutdown(&self) -> Result<(), Report<ChatServiceError>> {
        self.engine
            .shutdown()
            .await
            .map_err(|error| Report::new(error).escalate(ChatServiceError::Shutdown))
    }
}

/// Failure while opening the workspace chat feature.
#[derive(Debug, Error)]
pub enum ChatServiceError {
    /// The attachment tree could not be prepared for pod-user access.
    #[error("failed to prepare chat attachment storage: {0}")]
    Attachment(String),
    /// Durable chat state could not be opened.
    #[error("failed to open durable chat state: {0}")]
    State(String),
    /// Workspace harness state could not be prepared.
    #[error("failed to open chat harness state")]
    Harnesses,
    /// Runtime bindings or durable state could not be shut down cleanly.
    #[error("failed to shut down workspace chats")]
    Shutdown,
}
