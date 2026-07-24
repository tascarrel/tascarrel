//! Control-plane implementation for repository inventory and approvals.

use async_trait::async_trait;
use reportify::Report;
use tascarrel_api::types::protocol as wire;
use tascarrel_api::types::repositories as api;

use crate::control_plane::InvocationCtx;
use crate::control_plane::SubscriptionCtx;
use crate::control_plane::operation_error_details;
use crate::control_plane::operations::EventSource;
use crate::control_plane::operations::ExecuteAction;
use crate::control_plane::operations::OpenSubscription;
use crate::services::repositories::RepositoryApprovalSubscription;
use crate::services::repositories::RepositoryPushStatusSubscription;
use crate::services::repositories::RepositoryServiceError;
use crate::services::repositories::RepositorySubscription;

#[async_trait]
impl ExecuteAction for api::PrepareRepositorySnapshotAction {
    async fn check_permissions(
        &self,
        context: &InvocationCtx<'_>,
    ) -> Result<(), Report<wire::OperationError>> {
        let caller = &context.require_routing_context()?.caller;
        if caller.is_host_or_client()
            || matches!(
                caller,
                wire::Actor::Workspace(address) if address.workspace == self.workspace
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
            .repositories()
            .prepare_snapshot(self)
            .await
            .map_err(repository_error)
    }
}

#[async_trait]
impl ExecuteAction for api::RefreshRepositorySnapshotAction {
    async fn check_permissions(
        &self,
        context: &InvocationCtx<'_>,
    ) -> Result<(), Report<wire::OperationError>> {
        let caller = &context.require_routing_context()?.caller;
        if caller.is_host_or_client()
            || matches!(
                caller,
                wire::Actor::Workspace(address) if address.workspace == self.workspace
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
            .repositories()
            .refresh_snapshot(self)
            .await
            .map_err(repository_error)
    }
}

#[async_trait]
impl ExecuteAction for api::ResolveRepositoryApprovalAction {
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
            .repositories()
            .resolve_approval(self)
            .await
            .map_err(repository_error)
    }
}

#[async_trait]
impl OpenSubscription for api::RepositoryListChangedSubscription {
    async fn check_permissions(
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

    type Source = RepositorySubscription;

    async fn open(
        self,
        context: SubscriptionCtx<'_>,
    ) -> Result<Self::Source, Report<wire::OperationError>> {
        context
            .state()
            .repositories()
            .subscribe(self)
            .map_err(repository_error)
    }
}

#[async_trait]
impl EventSource for RepositorySubscription {
    type Event = api::RepositoryListChangedEvent;

    async fn recv(&mut self) -> Result<Option<Self::Event>, Report<wire::OperationError>> {
        RepositorySubscription::recv(self)
            .await
            .map(Some)
            .map_err(repository_error)
    }
}

#[async_trait]
impl OpenSubscription for api::RepositoryApprovalRequestListChangedSubscription {
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
            return Ok(());
        }
        Err(wire::OperationError::forbidden())
    }

    type Source = RepositoryApprovalSubscription;

    async fn open(
        self,
        context: SubscriptionCtx<'_>,
    ) -> Result<Self::Source, Report<wire::OperationError>> {
        context
            .state()
            .repositories()
            .subscribe_approvals(self)
            .map_err(repository_error)
    }
}

#[async_trait]
impl EventSource for RepositoryApprovalSubscription {
    type Event = api::RepositoryApprovalRequestListChangedEvent;

    async fn recv(&mut self) -> Result<Option<Self::Event>, Report<wire::OperationError>> {
        RepositoryApprovalSubscription::recv(self)
            .await
            .map(Some)
            .map_err(repository_error)
    }
}

#[async_trait]
impl OpenSubscription for api::RepositoryPushStatusChangedSubscription {
    async fn check_permissions(
        &self,
        context: &SubscriptionCtx<'_>,
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

    type Source = RepositoryPushStatusSubscription;

    async fn open(
        self,
        context: SubscriptionCtx<'_>,
    ) -> Result<Self::Source, Report<wire::OperationError>> {
        context
            .state()
            .repositories()
            .subscribe_push_status(self)
            .map_err(repository_error)
    }
}

#[async_trait]
impl EventSource for RepositoryPushStatusSubscription {
    type Event = api::RepositoryPushStatusChangedEvent;

    async fn recv(&mut self) -> Result<Option<Self::Event>, Report<wire::OperationError>> {
        RepositoryPushStatusSubscription::recv(self)
            .await
            .map(Some)
            .map_err(repository_error)
    }
}

fn repository_error(report: Report<RepositoryServiceError>) -> Report<wire::OperationError> {
    let details = operation_error_details(report.to_string());
    let error = match report.error() {
        RepositoryServiceError::InvalidRequest(_) => wire::OperationError::InvalidRequest(details),
        RepositoryServiceError::Unavailable(_) => wire::OperationError::Unavailable(details),
        RepositoryServiceError::InvalidConfiguration | RepositoryServiceError::Internal(_) => {
            wire::OperationError::Internal(details)
        }
    };
    report.escalate(error)
}
