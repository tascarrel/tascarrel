//! Control-plane implementation for overlay-share inspection and approval.

use async_trait::async_trait;
use reportify::Report;
use tascarrel_api::types::protocol as wire;
use tascarrel_api::types::shares as api;

use crate::control_plane::InvocationCtx;
use crate::control_plane::SubscriptionCtx;
use crate::control_plane::operation_error_details;
use crate::control_plane::operations::EventSource;
use crate::control_plane::operations::ExecuteAction;
use crate::control_plane::operations::OpenSubscription;
use crate::services::share_overlays::ShareOverlayApprovalSubscription;
use crate::services::share_overlays::ShareOverlayServiceError;

macro_rules! implement_action {
    ($action:ty, $method:ident) => {
        #[async_trait]
        impl ExecuteAction for $action {
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
                context
                    .state()
                    .share_overlays()
                    .$method(context.state().workspaces(), self)
                    .await
                    .map_err(overlay_error)
            }
        }
    };
}

implement_action!(api::InspectShareOverlayAction, inspect);
implement_action!(api::ApplyShareOverlayAction, apply);

#[async_trait]
impl ExecuteAction for api::RequestShareOverlayApprovalAction {
    async fn check_permissions(
        &self,
        context: &InvocationCtx<'_>,
    ) -> Result<(), Report<wire::OperationError>> {
        if matches!(
            &context.require_routing_context()?.caller,
            wire::Actor::Pod(address)
                if address.workspace == self.workspace && address.pod_id == self.pod_id
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
        context
            .state()
            .share_overlays()
            .request_approval(context.state().workspaces(), self)
            .await
            .map_err(overlay_error)
    }
}

#[async_trait]
impl ExecuteAction for api::CancelShareOverlayApprovalAction {
    async fn check_permissions(
        &self,
        context: &InvocationCtx<'_>,
    ) -> Result<(), Report<wire::OperationError>> {
        let caller = &context.require_routing_context()?.caller;
        if caller.is_host_or_client()
            || matches!(
                caller,
                wire::Actor::Pod(address)
                    if address.workspace == self.workspace && address.pod_id == self.pod_id
            )
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
        context
            .state()
            .share_overlays()
            .cancel_approval(self)
            .await
            .map_err(overlay_error)
    }
}

#[async_trait]
impl ExecuteAction for api::ResolveShareOverlayApprovalAction {
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
        context
            .state()
            .share_overlays()
            .resolve_approval(context.state().workspaces(), self)
            .await
            .map_err(overlay_error)
    }
}

#[async_trait]
impl OpenSubscription for api::ShareOverlayApprovalRequestListChangedSubscription {
    async fn check_permissions(
        &self,
        context: &SubscriptionCtx<'_>,
    ) -> Result<(), Report<wire::OperationError>> {
        let caller = &context.require_routing_context()?.caller;
        if caller.is_host_or_client()
            || matches!(
                (caller, self.pod_id.as_ref()),
                (wire::Actor::Pod(address), Some(pod_id))
                    if address.workspace == self.workspace && &address.pod_id == pod_id
            )
        {
            Ok(())
        } else {
            Err(wire::OperationError::forbidden())
        }
    }

    type Source = ShareOverlayApprovalSubscription;

    async fn open(
        self,
        context: SubscriptionCtx<'_>,
    ) -> Result<Self::Source, Report<wire::OperationError>> {
        Ok(context.state().share_overlays().subscribe(self))
    }
}

#[async_trait]
impl EventSource for ShareOverlayApprovalSubscription {
    type Event = api::ShareOverlayApprovalRequestListChangedEvent;

    async fn recv(&mut self) -> Result<Option<Self::Event>, Report<wire::OperationError>> {
        ShareOverlayApprovalSubscription::recv(self)
            .await
            .map(Some)
            .map_err(overlay_error)
    }
}

fn overlay_error(report: Report<ShareOverlayServiceError>) -> Report<wire::OperationError> {
    let details = operation_error_details(report.to_string());
    let error = match report.error() {
        ShareOverlayServiceError::InvalidRequest(_) => {
            wire::OperationError::InvalidRequest(details)
        }
        ShareOverlayServiceError::Unavailable(_) => wire::OperationError::Unavailable(details),
        ShareOverlayServiceError::Internal(_) => wire::OperationError::Internal(details),
    };
    report.escalate(error)
}
