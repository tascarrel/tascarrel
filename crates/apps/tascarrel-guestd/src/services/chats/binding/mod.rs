//! Runtime bindings between durable chats and running harness sessions.

use std::sync::Arc;

use futures_util::future::BoxFuture;
use tascarrel_api::ArcVec;
use tascarrel_api::ids::ChatBindingId;
use tascarrel_api::ids::ChatId;
use tascarrel_api::types::chats::ChatHarness;
use tascarrel_api::types::pods::PodId;

use crate::GuestNetworkService;
use crate::ProcessSupervisor;
use crate::services::chats::harness::protocol::HarnessCommand;
use crate::services::chats::harness::protocol::HarnessCommandResult;
use crate::services::chats::harness::protocol::HarnessEvent;
use crate::services::chats::state::protocol::HarnessResumption;
use crate::services::pods::PodService;

/// Creates runtime harness bindings for chats.
pub trait BindingProvider: Send + Sync {
    /// Returns the complete workspace-wide harness list.
    fn harnesses(&self) -> BoxFuture<'_, Result<ArcVec<ChatHarness>, HarnessBindingError>>;

    /// Attaches a running harness binding to a chat.
    fn attach(
        &self,
        request: AttachHarnessBindingRequest,
        processes: ProcessSupervisor,
        pods: PodService,
        network_service: Arc<GuestNetworkService>,
    ) -> BoxFuture<'_, Result<HarnessBinding, HarnessBindingError>>;
}

/// Information needed to attach a runtime harness binding to a chat.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct AttachHarnessBindingRequest {
    /// Identifier assigned to this binding attempt by the engine.
    pub binding_id: ChatBindingId,
    /// Chat to which the binding belongs.
    pub chat_id: ChatId,
    /// Pod in which the harness must run.
    pub pod_id: PodId,
    /// Durable harness state from which the native conversation should
    /// continue.
    pub resumption: HarnessResumption,
}

/// Independently usable control and event handles for an attached harness
/// binding.
pub struct HarnessBinding {
    /// Shareable handle used to send commands and detach the binding.
    pub control: Arc<dyn HarnessBindingControl>,
    /// Ordered, single-consumer stream of normalized harness events.
    pub events: Box<dyn HarnessBindingEventStream>,
}

/// Concurrent control interface for an attached harness binding.
pub trait HarnessBindingControl: Send + Sync {
    /// Applies a command to the running harness session.
    fn apply(
        &self,
        command: HarnessCommand,
    ) -> BoxFuture<'_, Result<HarnessCommandResult, HarnessBindingError>>;

    /// Stops the binding and releases its runtime resources.
    ///
    /// Implementations must treat repeated calls as successful once detachment
    /// has begun.
    fn detach(&self) -> BoxFuture<'_, Result<(), HarnessBindingError>>;
}

/// Single-consumer event interface for an attached harness binding.
pub trait HarnessBindingEventStream: Send {
    /// Waits for the next normalized harness event.
    ///
    /// The stream returns `None` after the binding has ended cleanly. An
    /// unexpected loss of the binding is returned as an error.
    fn next_event(&mut self) -> BoxFuture<'_, Result<Option<HarnessEvent>, HarnessBindingError>>;
}

/// Failure to create, control, or consume a harness binding.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct HarnessBindingError {
    /// Stable provider-defined error code.
    pub code: String,
    /// Human-readable explanation safe to expose to clients.
    pub message: String,
}
