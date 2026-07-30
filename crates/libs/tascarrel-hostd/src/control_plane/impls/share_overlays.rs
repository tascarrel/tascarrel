//! Control-plane implementation for overlay-share inspection and approval.

use async_trait::async_trait;
use reportify::Report;
use tascarrel_api::types::protocol as wire;
use tascarrel_api::types::shares as api;

use crate::control_plane::InvocationCtx;
use crate::control_plane::operation_error_details;
use crate::control_plane::operations::ExecuteAction;
use crate::services::share_overlays;
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
                share_overlays::$method(context.state().workspaces(), self)
                    .await
                    .map_err(overlay_error)
            }
        }
    };
}

implement_action!(api::InspectShareOverlayAction, inspect);
implement_action!(api::ApplyShareOverlayAction, apply);

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
