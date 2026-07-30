//! Control-plane implementation for durable workspace Automations.

use async_trait::async_trait;
use reportify::Report;
use tascarrel_api::types::automations as api;
use tascarrel_api::types::protocol as wire;

use crate::control_plane::InvocationCtx;
use crate::control_plane::SubscriptionCtx;
use crate::control_plane::operation_error_details;
use crate::control_plane::operations::EventSource;
use crate::control_plane::operations::ExecuteAction;
use crate::control_plane::operations::OpenSubscription;
use crate::services::automations::AutomationCatalogSubscription;
use crate::services::automations::AutomationExecutionSubscription;
use crate::services::automations::AutomationOutputSubscription;
use crate::services::automations::AutomationServiceError;

macro_rules! require_host_or_client {
    ($context:expr) => {
        if $context
            .require_routing_context()?
            .caller
            .is_host_or_client()
        {
            Ok(())
        } else {
            Err(wire::OperationError::forbidden())
        }
    };
}

#[async_trait]
impl ExecuteAction for api::StartAutomationAction {
    async fn check_permissions(
        &self,
        context: &InvocationCtx<'_>,
    ) -> Result<(), Report<wire::OperationError>> {
        require_host_or_client!(context)
    }

    async fn execute(
        self,
        context: InvocationCtx<'_>,
    ) -> Result<Self::Output, Report<wire::OperationError>> {
        let actor = context.require_routing_context()?.caller.clone();
        context
            .state()
            .automations()
            .start(context.state().config(), self, actor)
            .await
            .map_err(automation_error)
    }
}

#[async_trait]
impl ExecuteAction for api::CancelAutomationExecutionAction {
    async fn check_permissions(
        &self,
        context: &InvocationCtx<'_>,
    ) -> Result<(), Report<wire::OperationError>> {
        require_host_or_client!(context)
    }

    async fn execute(
        self,
        context: InvocationCtx<'_>,
    ) -> Result<Self::Output, Report<wire::OperationError>> {
        context
            .state()
            .automations()
            .cancel(self)
            .await
            .map_err(automation_error)
    }
}

#[async_trait]
impl ExecuteAction for api::ResolveAutomationApprovalAction {
    async fn check_permissions(
        &self,
        context: &InvocationCtx<'_>,
    ) -> Result<(), Report<wire::OperationError>> {
        require_host_or_client!(context)
    }

    async fn execute(
        self,
        context: InvocationCtx<'_>,
    ) -> Result<Self::Output, Report<wire::OperationError>> {
        let actor = context.require_routing_context()?.caller.clone();
        context
            .state()
            .automations()
            .resolve_approval(self, actor)
            .await
            .map_err(automation_error)
    }
}

#[async_trait]
impl OpenSubscription for api::AutomationCatalogSubscription {
    async fn check_permissions(
        &self,
        context: &SubscriptionCtx<'_>,
    ) -> Result<(), Report<wire::OperationError>> {
        require_host_or_client!(context)
    }

    type Source = AutomationCatalogSubscription;

    async fn open(
        self,
        context: SubscriptionCtx<'_>,
    ) -> Result<Self::Source, Report<wire::OperationError>> {
        context
            .state()
            .automations()
            .subscribe_catalog(context.state().config().clone(), self)
            .await
            .map_err(automation_error)
    }
}

#[async_trait]
impl EventSource for AutomationCatalogSubscription {
    type Event = api::AutomationCatalogEvent;

    async fn recv(&mut self) -> Result<Option<Self::Event>, Report<wire::OperationError>> {
        AutomationCatalogSubscription::recv(self)
            .await
            .map(Some)
            .map_err(automation_error)
    }
}

#[async_trait]
impl OpenSubscription for api::AutomationExecutionListSubscription {
    async fn check_permissions(
        &self,
        context: &SubscriptionCtx<'_>,
    ) -> Result<(), Report<wire::OperationError>> {
        require_host_or_client!(context)
    }

    type Source = AutomationExecutionSubscription;

    async fn open(
        self,
        context: SubscriptionCtx<'_>,
    ) -> Result<Self::Source, Report<wire::OperationError>> {
        Ok(context.state().automations().subscribe_executions(self))
    }
}

#[async_trait]
impl EventSource for AutomationExecutionSubscription {
    type Event = api::AutomationExecutionListEvent;

    async fn recv(&mut self) -> Result<Option<Self::Event>, Report<wire::OperationError>> {
        AutomationExecutionSubscription::recv(self)
            .await
            .map(Some)
            .map_err(automation_error)
    }
}

#[async_trait]
impl OpenSubscription for api::AutomationOutputSubscription {
    async fn check_permissions(
        &self,
        context: &SubscriptionCtx<'_>,
    ) -> Result<(), Report<wire::OperationError>> {
        require_host_or_client!(context)
    }

    type Source = AutomationOutputSubscription;

    async fn open(
        self,
        context: SubscriptionCtx<'_>,
    ) -> Result<Self::Source, Report<wire::OperationError>> {
        context
            .state()
            .automations()
            .subscribe_output(self)
            .await
            .map_err(automation_error)
    }
}

#[async_trait]
impl EventSource for AutomationOutputSubscription {
    type Event = api::AutomationOutputEvent;

    async fn recv(&mut self) -> Result<Option<Self::Event>, Report<wire::OperationError>> {
        AutomationOutputSubscription::recv(self)
            .await
            .map(Some)
            .map_err(automation_error)
    }
}

fn automation_error(report: Report<AutomationServiceError>) -> Report<wire::OperationError> {
    let details = operation_error_details(report.to_string());
    match report.error() {
        AutomationServiceError::InvalidRequest(_) | AutomationServiceError::NotFound => {
            report.escalate(wire::OperationError::InvalidRequest(details))
        }
        AutomationServiceError::Unavailable(_) => {
            report.escalate(wire::OperationError::Unavailable(details))
        }
        AutomationServiceError::Internal(_) => {
            report.escalate(wire::OperationError::Internal(details))
        }
    }
}
