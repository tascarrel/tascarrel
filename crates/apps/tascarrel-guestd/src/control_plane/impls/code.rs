//! Control-plane implementation for guest-owned Code editor sessions.

use async_trait::async_trait;
use reportify::Report;
use tascarrel_api::types::code as api;
use tascarrel_api::types::protocol as wire;

use crate::CodeServiceError;
use crate::control_plane::InvocationCtx;
use crate::control_plane::SubscriptionCtx;
use crate::control_plane::operation_error_details;
use crate::control_plane::operations::EventSource;
use crate::control_plane::operations::ExecuteAction;
use crate::control_plane::operations::OpenSubscription;
use crate::control_plane::operations::store_event;
use crate::services::code::CodeSessionListSubscription;

#[async_trait]
impl ExecuteAction for api::EnsureCodeSessionAction {
    fn check_permissions(
        &self,
        context: &InvocationCtx<'_>,
    ) -> Result<(), Report<wire::OperationError>> {
        require_host_or_client(&context.require_routing_context()?.caller)?;
        require_workspace(context.target_workspace()?, &self.workspace)
    }

    async fn execute(
        self,
        context: InvocationCtx<'_>,
    ) -> Result<Self::Output, Report<wire::OperationError>> {
        let caller = context.require_routing_context()?.caller.clone();
        let request_context = context.nested_host_request_context()?;
        context
            .state()
            .code()
            .ensure_session(
                self,
                caller,
                context.state().pods(),
                context.state().processes(),
                std::sync::Arc::clone(context.state().network()),
                context.host(),
                request_context,
            )
            .await
            .map_err(code_error)
    }
}

#[async_trait]
impl ExecuteAction for api::DeleteCodeSessionAction {
    fn check_permissions(
        &self,
        context: &InvocationCtx<'_>,
    ) -> Result<(), Report<wire::OperationError>> {
        require_host_or_client(&context.require_routing_context()?.caller)?;
        context.target_workspace().map(|_| ())
    }

    async fn execute(
        self,
        context: InvocationCtx<'_>,
    ) -> Result<Self::Output, Report<wire::OperationError>> {
        let request_context = context.nested_host_request_context()?;
        context
            .state()
            .code()
            .delete_session(
                &self,
                context.state().processes(),
                context.host(),
                request_context,
            )
            .await
            .map_err(code_error)
    }
}

#[async_trait]
impl OpenSubscription for api::CodeSessionListChangedSubscription {
    fn check_permissions(
        &self,
        context: &SubscriptionCtx<'_>,
    ) -> Result<(), Report<wire::OperationError>> {
        require_host_or_client(&context.require_routing_context()?.caller)?;
        require_workspace(context.target_workspace()?, &self.workspace)
    }

    type Source = CodeSessionListSubscription;

    async fn open(
        self,
        context: SubscriptionCtx<'_>,
    ) -> Result<Self::Source, Report<wire::OperationError>> {
        context
            .state()
            .code()
            .subscribe_sessions(&self)
            .map_err(code_error)
    }
}

#[async_trait]
impl EventSource for CodeSessionListSubscription {
    type Event = api::CodeSessionListChangedEvent;

    async fn recv(&mut self) -> Result<Option<Self::Event>, Report<wire::OperationError>> {
        Ok(tascarrel_store::Subscription::recv(self)
            .await
            .map(|change| api::CodeSessionListChangedEvent {
                change: store_event(change),
            }))
    }
}

fn require_host_or_client(caller: &wire::Actor) -> Result<(), Report<wire::OperationError>> {
    if caller.is_host_or_client() {
        Ok(())
    } else {
        Err(wire::OperationError::forbidden())
    }
}

fn require_workspace(
    addressed: &tascarrel_api::types::workspaces::WorkspaceName,
    requested: &tascarrel_api::types::workspaces::WorkspaceName,
) -> Result<(), Report<wire::OperationError>> {
    if addressed == requested {
        Ok(())
    } else {
        Err(wire::OperationError::forbidden())
    }
}

fn code_error(report: Report<CodeServiceError>) -> Report<wire::OperationError> {
    let details = operation_error_details(report.to_string());
    let error = match report.error() {
        CodeServiceError::InvalidConfiguration | CodeServiceError::Internal(_) => {
            wire::OperationError::Internal(details)
        }
        CodeServiceError::InvalidRequest(_) => wire::OperationError::InvalidRequest(details),
        CodeServiceError::Unavailable(_) => wire::OperationError::Unavailable(details),
        CodeServiceError::Overloaded(_) => wire::OperationError::Overloaded(details),
    };
    report.escalate(error)
}
