//! Control-plane implementation for repository status and detailed changes.

use async_trait::async_trait;
use reportify::Report;
use tascarrel_api::types::changes as api;
use tascarrel_api::types::protocol as wire;

use crate::control_plane::InvocationCtx;
use crate::control_plane::SubscriptionCtx;
use crate::control_plane::operation_error_details;
use crate::control_plane::operations::EventSource;
use crate::control_plane::operations::ExecuteAction;
use crate::control_plane::operations::OpenSubscription;
use crate::control_plane::operations::store_event;
use crate::services::changes::ChangesServiceError;
use crate::services::changes::RepositoryStatusListSubscription;

#[async_trait]
impl ExecuteAction for api::GetDivergentCommitsAction {
    fn check_permissions(
        &self,
        context: &InvocationCtx<'_>,
    ) -> Result<(), Report<wire::OperationError>> {
        allow_host_or_client(
            context
                .require_routing_context()?
                .caller
                .is_host_or_client(),
        )
    }

    async fn execute(
        self,
        context: InvocationCtx<'_>,
    ) -> Result<Self::Output, Report<wire::OperationError>> {
        ensure_tracking(context.state()).await;
        context
            .state()
            .changes()
            .divergent_commits(self)
            .await
            .map_err(changes_error)
    }
}

#[async_trait]
impl ExecuteAction for api::GetChangeSetAction {
    fn check_permissions(
        &self,
        context: &InvocationCtx<'_>,
    ) -> Result<(), Report<wire::OperationError>> {
        allow_host_or_client(
            context
                .require_routing_context()?
                .caller
                .is_host_or_client(),
        )
    }

    async fn execute(
        self,
        context: InvocationCtx<'_>,
    ) -> Result<Self::Output, Report<wire::OperationError>> {
        ensure_tracking(context.state()).await;
        context
            .state()
            .changes()
            .change_set(self)
            .await
            .map_err(changes_error)
    }
}

#[async_trait]
impl OpenSubscription for api::RepositoryStatusListChangedSubscription {
    fn check_permissions(
        &self,
        context: &SubscriptionCtx<'_>,
    ) -> Result<(), Report<wire::OperationError>> {
        allow_host_or_client(
            context
                .require_routing_context()?
                .caller
                .is_host_or_client(),
        )
    }

    type Source = RepositoryStatusListSubscription;

    async fn open(
        self,
        context: SubscriptionCtx<'_>,
    ) -> Result<Self::Source, Report<wire::OperationError>> {
        ensure_tracking(context.state()).await;
        context
            .state()
            .changes()
            .subscribe(&self)
            .map_err(changes_error)
    }
}

#[async_trait]
impl EventSource for RepositoryStatusListSubscription {
    type Event = api::RepositoryStatusListChangedEvent;

    async fn recv(&mut self) -> Result<Option<Self::Event>, Report<wire::OperationError>> {
        Ok(tascarrel_store::Subscription::recv(self)
            .await
            .map(|event| api::RepositoryStatusListChangedEvent {
                change: store_event(event),
            }))
    }
}

async fn ensure_tracking(state: &crate::control_plane::GuestState) {
    state
        .changes()
        .ensure_tracking(
            state.pods().clone(),
            state.repositories().cloned(),
            state.repository_config().cloned(),
        )
        .await;
}

fn allow_host_or_client(allowed: bool) -> Result<(), Report<wire::OperationError>> {
    if allowed {
        Ok(())
    } else {
        Err(wire::OperationError::forbidden())
    }
}

pub(crate) fn changes_error(report: Report<ChangesServiceError>) -> Report<wire::OperationError> {
    let error = match report.error() {
        ChangesServiceError::InvalidRequest(message) => {
            wire::OperationError::InvalidRequest(operation_error_details(message.clone()))
        }
        ChangesServiceError::Unavailable(message) => {
            wire::OperationError::Unavailable(operation_error_details(message.clone()))
        }
        ChangesServiceError::Internal(message) => {
            wire::OperationError::Internal(operation_error_details(message.clone()))
        }
    };
    report.escalate(error)
}
