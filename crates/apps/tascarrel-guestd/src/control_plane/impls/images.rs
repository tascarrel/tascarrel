//! Control-plane implementations for the workspace image schema.

use async_trait::async_trait;
use reportify::Report;
use tascarrel_api::types::images as api;
use tascarrel_api::types::protocol as wire;

use crate::control_plane::InvocationCtx;
use crate::control_plane::SubscriptionCtx;
use crate::control_plane::operation_error_details;
use crate::control_plane::operations::EventSource;
use crate::control_plane::operations::ExecuteAction;
use crate::control_plane::operations::OpenSubscription;
use crate::control_plane::operations::store_event;
use crate::services::images::ImageListSubscription;
use crate::services::images::ImageServiceError;
use crate::services::images::LogSubscription;

#[async_trait]
impl ExecuteAction for api::BuildImageAction {
    fn check_permissions(
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
        let repositories = context.repository_preparation().await?;
        context
            .state()
            .images()
            .build(
                self,
                context.state().pods(),
                context.state().processes(),
                std::sync::Arc::clone(context.state().network()),
                context.state().image_input().as_ref(),
                repositories,
            )
            .await
            .map_err(image_error)
    }
}

#[async_trait]
impl ExecuteAction for api::UpdateImageWorkspaceSeedAction {
    fn check_permissions(
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
        let repositories = context.repository_preparation().await?;
        context
            .state()
            .images()
            .update_workspace_seed(self, repositories)
            .await
            .map_err(image_error)
    }
}

#[async_trait]
impl OpenSubscription for api::ImageListChangedSubscription {
    fn check_permissions(
        &self,
        context: &SubscriptionCtx<'_>,
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

    type Source = ImageListSubscription;

    async fn open(
        self,
        context: SubscriptionCtx<'_>,
    ) -> Result<Self::Source, Report<wire::OperationError>> {
        context
            .state()
            .images()
            .subscribe_image_list(&self)
            .map_err(image_error)
    }
}

#[async_trait]
impl EventSource for ImageListSubscription {
    type Event = api::ImageListChangedEvent;

    async fn recv(&mut self) -> Result<Option<Self::Event>, Report<wire::OperationError>> {
        Ok(tascarrel_store::Subscription::recv(self)
            .await
            .map(|event| api::ImageListChangedEvent {
                change: store_event(event),
            }))
    }
}

#[async_trait]
impl OpenSubscription for api::ImageLogSubscription {
    fn check_permissions(
        &self,
        context: &SubscriptionCtx<'_>,
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

    type Source = LogSubscription;

    async fn open(
        self,
        context: SubscriptionCtx<'_>,
    ) -> Result<Self::Source, Report<wire::OperationError>> {
        context
            .state()
            .images()
            .subscribe_log(self)
            .map_err(image_error)
    }
}

#[async_trait]
impl EventSource for LogSubscription {
    type Event = api::ImageLogEvent;

    async fn recv(&mut self) -> Result<Option<Self::Event>, Report<wire::OperationError>> {
        Ok(LogSubscription::recv(self).await)
    }
}

/// Maps an image service report to a peer-visible operation error.
fn image_error(report: Report<ImageServiceError>) -> Report<wire::OperationError> {
    let error = match report.error() {
        ImageServiceError::InvalidRequest(message) => {
            wire::OperationError::InvalidRequest(operation_error_details(message.clone()))
        }
        ImageServiceError::Internal(message) => {
            wire::OperationError::Internal(operation_error_details(message.clone()))
        }
    };
    report.escalate(error)
}
