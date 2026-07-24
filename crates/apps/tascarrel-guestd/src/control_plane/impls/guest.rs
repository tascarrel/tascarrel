//! Control-plane implementations for guest information and resource metrics.

use async_trait::async_trait;
use reportify::Report;
use tascarrel_api::types::guest as api;
use tascarrel_api::types::protocol as wire;

use crate::control_plane::InvocationCtx;
use crate::control_plane::SubscriptionCtx;
use crate::control_plane::operations::EventSource;
use crate::control_plane::operations::ExecuteAction;
use crate::control_plane::operations::OpenSubscription;
use crate::services::guest::GuestMetricsSubscription;

#[async_trait]
impl ExecuteAction for api::QueryGuestInformationAction {
    fn check_permissions(
        &self,
        context: &InvocationCtx<'_>,
    ) -> Result<(), Report<wire::OperationError>> {
        context.require_routing_context()?;
        Ok(())
    }

    async fn execute(
        self,
        context: InvocationCtx<'_>,
    ) -> Result<Self::Output, Report<wire::OperationError>> {
        Ok(context.state().guest().information())
    }
}

#[async_trait]
impl OpenSubscription for api::GuestMetricsSubscription {
    fn check_permissions(
        &self,
        context: &SubscriptionCtx<'_>,
    ) -> Result<(), Report<wire::OperationError>> {
        context.require_routing_context()?;
        Ok(())
    }

    type Source = GuestMetricsSubscription;

    async fn open(
        self,
        context: SubscriptionCtx<'_>,
    ) -> Result<Self::Source, Report<wire::OperationError>> {
        Ok(context.state().guest().subscribe(self))
    }
}

#[async_trait]
impl EventSource for GuestMetricsSubscription {
    type Event = api::GuestMetricsEvent;

    async fn recv(&mut self) -> Result<Option<Self::Event>, Report<wire::OperationError>> {
        Ok(GuestMetricsSubscription::recv(self).await)
    }
}
