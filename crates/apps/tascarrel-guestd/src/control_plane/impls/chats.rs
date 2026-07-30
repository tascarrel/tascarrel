//! Control-plane implementations for workspace chats and harnesses.

use async_trait::async_trait;
use reportify::ErrorExt as _;
use reportify::Report;
use tascarrel_api::types::chats as api;
use tascarrel_api::types::config as config_api;
use tascarrel_api::types::pods as pod_api;
use tascarrel_api::types::protocol as wire;

use crate::control_plane::InvocationCtx;
use crate::control_plane::SubscriptionCtx;
use crate::control_plane::chat_subscription::ChatEventSource;
use crate::control_plane::operation_error_details;
use crate::control_plane::operations::EventSource;
use crate::control_plane::operations::ExecuteAction;
use crate::control_plane::operations::OpenSubscription;
use crate::control_plane::operations::store_event;
use crate::services::chats::ChatEngineError;
use crate::services::chats::ChatListStoreSubscription;
use crate::services::chats::HarnessListSubscription;
use crate::services::chats::HarnessManagerError;
use crate::services::chats::UsageReportSubscription;
use crate::services::pods::PodServiceError;

macro_rules! chat_permissions {
    () => {
        fn check_permissions(
            &self,
            context: &InvocationCtx<'_>,
        ) -> Result<(), Report<wire::OperationError>> {
            require_client(context)
        }
    };
}

macro_rules! chat_subscription_permissions {
    () => {
        fn check_permissions(
            &self,
            context: &SubscriptionCtx<'_>,
        ) -> Result<(), Report<wire::OperationError>> {
            require_subscriber(context)
        }
    };
}

#[async_trait]
impl ExecuteAction for api::GetPodChatsAction {
    fn check_permissions(
        &self,
        context: &InvocationCtx<'_>,
    ) -> Result<(), Report<wire::OperationError>> {
        require_chat_pod_input(context, &self.pod_id)
    }

    async fn execute(
        self,
        context: InvocationCtx<'_>,
    ) -> Result<Self::Output, Report<wire::OperationError>> {
        context
            .state()
            .chats()
            .engine()
            .get_pod_chats(self.pod_id)
            .await
            .map_err(chat_error)
    }
}

#[async_trait]
impl ExecuteAction for api::GetChatUsageReportAction {
    chat_permissions!();

    async fn execute(
        self,
        context: InvocationCtx<'_>,
    ) -> Result<Self::Output, Report<wire::OperationError>> {
        context
            .state()
            .chats()
            .engine()
            .get_usage_report(&self)
            .await
            .map_err(chat_error)
    }
}

#[async_trait]
impl ExecuteAction for api::CreateChatAction {
    chat_permissions!();

    async fn execute(
        self,
        context: InvocationCtx<'_>,
    ) -> Result<Self::Output, Report<wire::OperationError>> {
        prepare_tasci(
            &context,
            &self.harness,
            self.model.as_ref().map(|model| model.model.as_ref()),
        )
        .await?;
        context
            .state()
            .chats()
            .engine()
            .create_chat(
                self,
                context.state().processes().clone(),
                context.state().pods().clone(),
                context.state().network().clone(),
            )
            .await
            .map_err(chat_error)
    }
}

#[async_trait]
impl ExecuteAction for api::CreatePodChatAction {
    chat_permissions!();

    async fn execute(
        self,
        context: InvocationCtx<'_>,
    ) -> Result<Self::Output, Report<wire::OperationError>> {
        prepare_tasci(
            &context,
            &self.harness,
            self.model.as_ref().map(|model| model.model.as_ref()),
        )
        .await?;
        let engine = context.state().chats().engine();
        engine
            .validate_chat_selection(&self.harness, self.model.as_ref())
            .await
            .map_err(chat_error)?;
        let title = self
            .title
            .as_deref()
            .map(str::trim)
            .filter(|title| !title.is_empty())
            .map_or_else(
                || crate::services::chats::fallback_title(&self.initial_prompt),
                str::to_owned,
            );
        let repository_preparation = context.repository_preparation_task()?;
        let pod = context
            .state()
            .pods()
            .create_with_repository_preparation_task(
                pod_api::CreatePodAction {
                    title: Some(title.into()),
                },
                context.state().images(),
                context.state().processes(),
                context.state().network().clone(),
                context.state().image_input().clone(),
                async move {
                    repository_preparation
                        .await
                        .map_err(pod_repository_preparation_error)
                },
            )
            .map_err(pod_error)?;
        let chat = engine
            .create_pod_chat(
                api::CreateChatAction {
                    pod_id: pod.pod_id.clone(),
                    cost_center_id: self.cost_center_id,
                    harness: self.harness,
                    title: self.title,
                    model: self.model,
                    initial_prompt: Some(self.initial_prompt),
                    auto_attach: Some(true),
                },
                context.state().processes().clone(),
                context.state().pods().clone(),
                context.state().network().clone(),
            )
            .await
            .map_err(chat_error)?;
        Ok(api::CreatePodChatOutput {
            pod_id: pod.pod_id,
            chat_id: chat.chat_id,
        })
    }
}

#[async_trait]
impl ExecuteAction for api::SetChatCostCenterAction {
    chat_permissions!();

    async fn execute(
        self,
        context: InvocationCtx<'_>,
    ) -> Result<Self::Output, Report<wire::OperationError>> {
        context
            .state()
            .chats()
            .engine()
            .set_cost_center(self)
            .await
            .map_err(chat_error)
    }
}

#[async_trait]
impl ExecuteAction for api::AttachChatBindingAction {
    chat_permissions!();

    async fn execute(
        self,
        context: InvocationCtx<'_>,
    ) -> Result<Self::Output, Report<wire::OperationError>> {
        let selection = context
            .state()
            .chats()
            .engine()
            .chat_selection(&self.chat_id)
            .await
            .map_err(chat_error)?;
        if let Some((harness, model)) = selection {
            prepare_tasci(
                &context,
                &harness,
                model.as_ref().map(|model| model.model.as_ref()),
            )
            .await?;
        }
        context
            .state()
            .chats()
            .engine()
            .attach_chat_binding(
                self,
                context.state().processes().clone(),
                context.state().pods().clone(),
                context.state().network().clone(),
            )
            .await
            .map_err(chat_error)
    }
}

macro_rules! engine_action {
    ($action:ty, $method:ident) => {
        #[async_trait]
        impl ExecuteAction for $action {
            chat_permissions!();

            async fn execute(
                self,
                context: InvocationCtx<'_>,
            ) -> Result<Self::Output, Report<wire::OperationError>> {
                context
                    .state()
                    .chats()
                    .engine()
                    .$method(self)
                    .await
                    .map_err(chat_error)
            }
        }
    };
}

engine_action!(api::DetachChatBindingAction, detach_chat_binding);
engine_action!(api::ArchiveChatAction, archive_chat);
engine_action!(
    api::AcknowledgeChatAttentionAction,
    acknowledge_chat_attention
);
engine_action!(api::FlushChatPromptQueueAction, flush_chat_prompt_queue);
engine_action!(api::RemoveChatQueuedPromptAction, remove_chat_queued_prompt);
engine_action!(api::InterruptChatAction, interrupt_chat);
engine_action!(api::CompactChatContextAction, compact_chat_context);
engine_action!(api::ResolveChatRequestAction, resolve_chat_request);

#[async_trait]
impl ExecuteAction for api::SendChatPromptAction {
    chat_permissions!();

    async fn execute(
        self,
        context: InvocationCtx<'_>,
    ) -> Result<Self::Output, Report<wire::OperationError>> {
        let selection = context
            .state()
            .chats()
            .engine()
            .chat_selection(&self.chat_id)
            .await
            .map_err(chat_error)?;
        if let Some((harness, model)) = selection {
            let requested_model = self.prompt.model.as_ref().or(model.as_ref());
            prepare_tasci(
                &context,
                &harness,
                requested_model.map(|model| model.model.as_ref()),
            )
            .await?;
        }
        context
            .state()
            .chats()
            .engine()
            .send_chat_prompt(
                self,
                context.state().processes().clone(),
                context.state().pods().clone(),
                context.state().network().clone(),
            )
            .await
            .map_err(chat_error)
    }
}

async fn prepare_tasci(
    context: &InvocationCtx<'_>,
    harness: &api::ChatHarnessKind,
    model: Option<&str>,
) -> Result<(), Report<wire::OperationError>> {
    if harness != &api::ChatHarnessKind::Tasci {
        return Ok(());
    }
    let workspace_name = context.target_workspace()?.clone();
    let output = context
        .host()
        .execute(
            context.nested_host_request_context()?,
            config_api::ResolveTasciModelAction {
                workspace_name,
                model: model.map(Into::into),
            },
        )
        .await
        .map_err(|error| {
            wire::OperationError::Unavailable(operation_error_details(error.to_string())).report()
        })?;
    context.state().chats().harnesses().configure_tasci(output);
    Ok(())
}

#[async_trait]
impl ExecuteAction for api::InstallChatHarnessAction {
    chat_permissions!();

    async fn execute(
        self,
        context: InvocationCtx<'_>,
    ) -> Result<Self::Output, Report<wire::OperationError>> {
        context
            .state()
            .chats()
            .harnesses()
            .install(self.harness)
            .await
            .map_err(harness_error)?;
        Ok(api::InstallChatHarnessOutput {})
    }
}

#[async_trait]
impl ExecuteAction for api::StartChatHarnessAuthAction {
    chat_permissions!();

    async fn execute(
        self,
        context: InvocationCtx<'_>,
    ) -> Result<Self::Output, Report<wire::OperationError>> {
        context
            .state()
            .chats()
            .harnesses()
            .start_auth(self.request)
            .await
            .map_err(harness_error)?;
        Ok(api::StartChatHarnessAuthOutput {})
    }
}

#[async_trait]
impl ExecuteAction for api::ValidateChatHarnessCredentialsAction {
    chat_permissions!();

    async fn execute(
        self,
        context: InvocationCtx<'_>,
    ) -> Result<Self::Output, Report<wire::OperationError>> {
        context
            .state()
            .chats()
            .harnesses()
            .schedule_credential_validation(self.harness)
            .map_err(harness_error)?;
        Ok(api::ValidateChatHarnessCredentialsOutput {})
    }
}

#[async_trait]
impl ExecuteAction for api::CancelChatHarnessAuthAction {
    chat_permissions!();

    async fn execute(
        self,
        context: InvocationCtx<'_>,
    ) -> Result<Self::Output, Report<wire::OperationError>> {
        context
            .state()
            .chats()
            .harnesses()
            .cancel_auth(self.harness)
            .await
            .map_err(harness_error)?;
        Ok(api::CancelChatHarnessAuthOutput {})
    }
}

#[async_trait]
impl ExecuteAction for api::LogoutChatHarnessAction {
    chat_permissions!();

    async fn execute(
        self,
        context: InvocationCtx<'_>,
    ) -> Result<Self::Output, Report<wire::OperationError>> {
        context
            .state()
            .chats()
            .harnesses()
            .logout(self.harness)
            .await
            .map_err(harness_error)?;
        Ok(api::LogoutChatHarnessOutput {})
    }
}

#[async_trait]
impl OpenSubscription for api::ChatListChangedSubscription {
    chat_subscription_permissions!();

    type Source = ChatListStoreSubscription;

    async fn open(
        self,
        context: SubscriptionCtx<'_>,
    ) -> Result<Self::Source, Report<wire::OperationError>> {
        context
            .state()
            .chats()
            .engine()
            .subscribe_chats(&self)
            .map_err(chat_error)
    }
}

#[async_trait]
impl EventSource for ChatListStoreSubscription {
    type Event = api::ChatListChangedEvent;

    async fn recv(&mut self) -> Result<Option<Self::Event>, Report<wire::OperationError>> {
        Ok(tascarrel_store::Subscription::recv(self)
            .await
            .map(|change| api::ChatListChangedEvent {
                change: store_event(change),
            }))
    }
}

#[async_trait]
impl OpenSubscription for api::ChatSubscription {
    fn check_permissions(
        &self,
        context: &SubscriptionCtx<'_>,
    ) -> Result<(), Report<wire::OperationError>> {
        require_chat_reader(context).map(|_| ())
    }

    type Source = ChatEventSource;

    async fn open(
        self,
        context: SubscriptionCtx<'_>,
    ) -> Result<Self::Source, Report<wire::OperationError>> {
        if let Some(pod_id) = require_chat_reader(&context)?
            && context
                .state()
                .chats()
                .engine()
                .chat_pod_id(&self.chat_id)
                .await
                .map_err(chat_error)?
                .as_ref()
                != Some(&pod_id)
        {
            return Err(wire::OperationError::forbidden());
        }
        let subscription = context
            .state()
            .chats()
            .engine()
            .subscribe_chat(self)
            .await
            .map_err(chat_error)?;
        Ok(ChatEventSource::new(subscription))
    }
}

#[async_trait]
impl OpenSubscription for api::ChatHarnessListSubscription {
    chat_subscription_permissions!();

    type Source = HarnessListSubscription;

    async fn open(
        self,
        context: SubscriptionCtx<'_>,
    ) -> Result<Self::Source, Report<wire::OperationError>> {
        Ok(context.state().chats().harnesses().subscribe())
    }
}

#[async_trait]
impl EventSource for HarnessListSubscription {
    type Event = api::ChatHarnessListEvent;

    async fn recv(&mut self) -> Result<Option<Self::Event>, Report<wire::OperationError>> {
        Ok(HarnessListSubscription::recv(self)
            .await
            .map(|harnesses| api::ChatHarnessListEvent { harnesses }))
    }
}

#[async_trait]
impl OpenSubscription for api::ChatUsageReportSubscription {
    chat_subscription_permissions!();

    type Source = UsageReportSubscription;

    async fn open(
        self,
        context: SubscriptionCtx<'_>,
    ) -> Result<Self::Source, Report<wire::OperationError>> {
        context
            .state()
            .chats()
            .engine()
            .subscribe_usage_report(self.from, self.until)
            .map_err(chat_error)
    }
}

#[async_trait]
impl EventSource for UsageReportSubscription {
    type Event = api::ChatUsageReportEvent;

    async fn recv(&mut self) -> Result<Option<Self::Event>, Report<wire::OperationError>> {
        UsageReportSubscription::recv(self)
            .await
            .map(|report| report.map(|report| api::ChatUsageReportEvent { report }))
            .map_err(|report| {
                report.escalate(wire::OperationError::Internal(operation_error_details(
                    "failed to load chat usage report",
                )))
            })
    }
}

fn require_client(context: &InvocationCtx<'_>) -> Result<(), Report<wire::OperationError>> {
    if context
        .require_routing_context()?
        .caller
        .is_host_or_client()
    {
        Ok(())
    } else {
        Err(wire::OperationError::forbidden())
    }
}

/// Authorizes a point-in-time query for the authenticated pod.
fn require_chat_pod_input(
    context: &InvocationCtx<'_>,
    pod_id: &tascarrel_api::types::pods::PodId,
) -> Result<(), Report<wire::OperationError>> {
    let routing = context.require_routing_context()?;
    if routing.caller.is_host_or_client() {
        return Ok(());
    }
    match &routing.caller {
        wire::Actor::Pod(address)
            if address == context.target_pod()? && address.pod_id == *pod_id =>
        {
            Ok(())
        }
        _ => Err(wire::OperationError::forbidden()),
    }
}

fn require_subscriber(context: &SubscriptionCtx<'_>) -> Result<(), Report<wire::OperationError>> {
    if context
        .require_routing_context()?
        .caller
        .is_host_or_client()
    {
        Ok(())
    } else {
        Err(wire::OperationError::forbidden())
    }
}

/// Authorizes host clients or returns the pod scope assigned to a pod reader.
fn require_chat_reader(
    context: &SubscriptionCtx<'_>,
) -> Result<Option<tascarrel_api::types::pods::PodId>, Report<wire::OperationError>> {
    let routing = context.require_routing_context()?;
    if routing.caller.is_host_or_client() {
        return Ok(None);
    }
    match &routing.caller {
        wire::Actor::Pod(address) if address == context.target_pod()? => {
            Ok(Some(address.pod_id.clone()))
        }
        _ => Err(wire::OperationError::forbidden()),
    }
}

fn chat_error(error: ChatEngineError) -> Report<wire::OperationError> {
    let operation_error = if matches!(
        error.code.as_str(),
        "invalid_request" | "InvalidInput" | "InvalidHarnessEvent"
    ) {
        wire::OperationError::InvalidRequest(operation_error_details(error.message.clone()))
    } else {
        wire::OperationError::Internal(operation_error_details(error.message.clone()))
    };
    Report::new(error).escalate(operation_error)
}

fn pod_error(report: Report<PodServiceError>) -> Report<wire::OperationError> {
    let error = match report.error() {
        PodServiceError::InvalidRequest(message) => {
            wire::OperationError::InvalidRequest(operation_error_details(message.clone()))
        }
        PodServiceError::Internal(message) => {
            wire::OperationError::Internal(operation_error_details(message.clone()))
        }
    };
    report.escalate(error)
}

/// Preserves repository preparation diagnostics under the pod service error
/// category.
fn pod_repository_preparation_error(
    error: Report<wire::OperationError>,
) -> Report<PodServiceError> {
    error.escalate(PodServiceError::Internal(
        "failed to prepare pod repositories".to_owned(),
    ))
}

fn harness_error(report: Report<HarnessManagerError>) -> Report<wire::OperationError> {
    let error = match report.error() {
        HarnessManagerError::InvalidRequest(message) => {
            wire::OperationError::InvalidRequest(operation_error_details(message.clone()))
        }
        HarnessManagerError::Internal(message) => {
            wire::OperationError::Internal(operation_error_details(message.clone()))
        }
    };
    report.escalate(error)
}
