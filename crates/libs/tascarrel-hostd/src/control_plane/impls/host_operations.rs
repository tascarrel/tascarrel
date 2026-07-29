//! Control-plane implementation for durable host operations.

use async_trait::async_trait;
use reportify::Report;
use tascarrel_api::types::host_operations as api;
use tascarrel_api::types::protocol as wire;

use crate::control_plane::InvocationCtx;
use crate::control_plane::SubscriptionCtx;
use crate::control_plane::operation_error_details;
use crate::control_plane::operations::EventSource;
use crate::control_plane::operations::ExecuteAction;
use crate::control_plane::operations::OpenSubscription;
use crate::services::host_operations::HostOperationAuditSubscription;
use crate::services::host_operations::HostOperationOutputSubscription;
use crate::services::host_operations::HostOperationServiceError;
use crate::services::host_operations::HostOperationSubscription;

#[async_trait]
impl ExecuteAction for api::RequestHostOperationAction {
    async fn check_permissions(
        &self,
        context: &InvocationCtx<'_>,
    ) -> Result<(), Report<wire::OperationError>> {
        if matches!(
            &context.require_routing_context()?.caller,
            wire::Actor::Pod(address) if address.workspace == self.workspace
        ) {
            Ok(())
        } else {
            Err(wire::OperationError::forbidden())
        }
    }

    async fn execute(
        self,
        context: InvocationCtx<'_>,
    ) -> Result<Self::Output, Report<wire::OperationError>> {
        let actor = context.require_routing_context()?.caller.clone();
        context
            .state()
            .host_operations()
            .request(self, actor, context.state().config())
            .await
            .map_err(host_operation_error)
    }
}

#[async_trait]
impl ExecuteAction for api::ResolveHostOperationAction {
    async fn check_permissions(
        &self,
        context: &InvocationCtx<'_>,
    ) -> Result<(), Report<wire::OperationError>> {
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

    async fn execute(
        self,
        context: InvocationCtx<'_>,
    ) -> Result<Self::Output, Report<wire::OperationError>> {
        let actor = context.require_routing_context()?.caller.clone();
        context
            .state()
            .host_operations()
            .resolve(self, actor)
            .await
            .map_err(host_operation_error)
    }
}

#[async_trait]
impl ExecuteAction for api::CancelHostOperationAction {
    async fn check_permissions(
        &self,
        context: &InvocationCtx<'_>,
    ) -> Result<(), Report<wire::OperationError>> {
        let caller = &context.require_routing_context()?.caller;
        if caller.is_host_or_client() {
            return Ok(());
        }
        let operation = context
            .state()
            .host_operations()
            .get(&self.operation_id)
            .await
            .map_err(host_operation_error)?;
        if matches!(
            caller,
            wire::Actor::Pod(address)
                if address.workspace == operation.workspace && address.pod_id == operation.pod_id
        ) {
            Ok(())
        } else {
            Err(wire::OperationError::forbidden())
        }
    }

    async fn execute(
        self,
        context: InvocationCtx<'_>,
    ) -> Result<Self::Output, Report<wire::OperationError>> {
        let actor = context.require_routing_context()?.caller.clone();
        context
            .state()
            .host_operations()
            .cancel(self, actor)
            .await
            .map_err(host_operation_error)
    }
}

#[async_trait]
impl OpenSubscription for api::HostOperationListChangedSubscription {
    async fn check_permissions(
        &self,
        context: &SubscriptionCtx<'_>,
    ) -> Result<(), Report<wire::OperationError>> {
        let caller = &context.require_routing_context()?.caller;
        if caller.is_host_or_client()
            || matches!(
                (caller, self.workspace.as_ref(), self.pod_id.as_ref()),
                (wire::Actor::Pod(address), Some(workspace), Some(pod_id))
                    if &address.workspace == workspace && &address.pod_id == pod_id
            )
        {
            Ok(())
        } else {
            Err(wire::OperationError::forbidden())
        }
    }

    type Source = HostOperationSubscription;

    async fn open(
        self,
        context: SubscriptionCtx<'_>,
    ) -> Result<Self::Source, Report<wire::OperationError>> {
        Ok(context.state().host_operations().subscribe(self))
    }
}

#[async_trait]
impl EventSource for HostOperationSubscription {
    type Event = api::HostOperationListChangedEvent;

    async fn recv(&mut self) -> Result<Option<Self::Event>, Report<wire::OperationError>> {
        HostOperationSubscription::recv(self)
            .await
            .map(Some)
            .map_err(host_operation_error)
    }
}

macro_rules! implement_operation_stream {
    (
        $input:ty,
        $source:ty,
        $event:ty,
        $open:ident,
        $receive:ident
    ) => {
        #[async_trait]
        impl OpenSubscription for $input {
            async fn check_permissions(
                &self,
                context: &SubscriptionCtx<'_>,
            ) -> Result<(), Report<wire::OperationError>> {
                let caller = &context.require_routing_context()?.caller;
                if caller.is_host_or_client() {
                    return Ok(());
                }
                let operation = context
                    .state()
                    .host_operations()
                    .get(&self.operation_id)
                    .await
                    .map_err(host_operation_error)?;
                if matches!(
                    caller,
                    wire::Actor::Pod(address)
                        if address.workspace == operation.workspace
                            && address.pod_id == operation.pod_id
                ) {
                    Ok(())
                } else {
                    Err(wire::OperationError::forbidden())
                }
            }

            type Source = $source;

            async fn open(
                self,
                context: SubscriptionCtx<'_>,
            ) -> Result<Self::Source, Report<wire::OperationError>> {
                context
                    .state()
                    .host_operations()
                    .$open(self)
                    .await
                    .map_err(host_operation_error)
            }
        }

        #[async_trait]
        impl EventSource for $source {
            type Event = $event;

            async fn recv(&mut self) -> Result<Option<Self::Event>, Report<wire::OperationError>> {
                <$source>::$receive(self)
                    .await
                    .map(Some)
                    .map_err(host_operation_error)
            }
        }
    };
}

implement_operation_stream!(
    api::HostOperationAuditSubscription,
    HostOperationAuditSubscription,
    api::HostOperationAuditEvent,
    subscribe_audit,
    recv
);
implement_operation_stream!(
    api::HostOperationOutputSubscription,
    HostOperationOutputSubscription,
    api::HostOperationOutputEvent,
    subscribe_output,
    recv
);

fn host_operation_error(report: Report<HostOperationServiceError>) -> Report<wire::OperationError> {
    let details = operation_error_details(report.to_string());
    let error = match report.error() {
        HostOperationServiceError::InvalidRequest(_)
        | HostOperationServiceError::InvalidConfiguration(_) => {
            wire::OperationError::InvalidRequest(details)
        }
        HostOperationServiceError::NotFound => wire::OperationError::InvalidRequest(details),
        HostOperationServiceError::Unavailable(_) => wire::OperationError::Unavailable(details),
        HostOperationServiceError::Internal(_) => wire::OperationError::Internal(details),
    };
    report.escalate(error)
}
