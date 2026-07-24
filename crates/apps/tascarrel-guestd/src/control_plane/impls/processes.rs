//! Control-plane implementations for the process supervision schema.

use async_trait::async_trait;
use reportify::Report;
use tascarrel_api::types::processes as api;
use tascarrel_api::types::protocol as wire;

use crate::ProcessSupervisorError;
use crate::control_plane::InvocationCtx;
use crate::control_plane::SubscriptionCtx;
use crate::control_plane::operation_error_details;
use crate::control_plane::operations::EventSource;
use crate::control_plane::operations::ExecuteAction;
use crate::control_plane::operations::OpenSubscription;
use crate::control_plane::operations::store_event;
use crate::services::processes::LogSubscription;
use crate::services::processes::ProcessListSubscription;
use crate::services::processes::TerminalSubscription;

#[async_trait]
impl ExecuteAction for api::GetPodProcessesAction {
    fn check_permissions(
        &self,
        context: &InvocationCtx<'_>,
    ) -> Result<(), Report<wire::OperationError>> {
        require_pod_input(context, &self.pod_id)
    }

    async fn execute(
        self,
        context: InvocationCtx<'_>,
    ) -> Result<Self::Output, Report<wire::OperationError>> {
        Ok(context.state().processes().get_pod_processes(&self.pod_id))
    }
}

#[async_trait]
impl ExecuteAction for api::SpawnProcessAction {
    fn check_permissions(
        &self,
        context: &InvocationCtx<'_>,
    ) -> Result<(), Report<wire::OperationError>> {
        require_pod_input(context, &self.pod_id)
    }

    async fn execute(
        self,
        context: InvocationCtx<'_>,
    ) -> Result<Self::Output, Report<wire::OperationError>> {
        let caller = context.require_routing_context()?.caller.clone();
        context
            .state()
            .processes()
            .spawn(
                self,
                caller,
                context.state().pods(),
                std::sync::Arc::clone(context.state().network()),
            )
            .map_err(process_error)
    }
}

#[async_trait]
impl ExecuteAction for api::SpawnProcessTerminalAction {
    fn check_permissions(
        &self,
        context: &InvocationCtx<'_>,
    ) -> Result<(), Report<wire::OperationError>> {
        require_pod_input(context, &self.pod_id)
    }

    async fn execute(
        self,
        context: InvocationCtx<'_>,
    ) -> Result<Self::Output, Report<wire::OperationError>> {
        let caller = context.require_routing_context()?.caller.clone();
        context
            .state()
            .processes()
            .spawn_terminal(
                self,
                caller,
                context.state().pods(),
                std::sync::Arc::clone(context.state().network()),
            )
            .map_err(process_error)
    }
}

#[async_trait]
impl ExecuteAction for api::KillProcessAction {
    fn check_permissions(
        &self,
        context: &InvocationCtx<'_>,
    ) -> Result<(), Report<wire::OperationError>> {
        require_process(context, &self.process_id)
    }

    async fn execute(
        self,
        context: InvocationCtx<'_>,
    ) -> Result<Self::Output, Report<wire::OperationError>> {
        context
            .state()
            .processes()
            .kill(self)
            .map_err(process_error)
    }
}

#[async_trait]
impl ExecuteAction for api::WriteProcessTerminalAction {
    fn check_permissions(
        &self,
        context: &InvocationCtx<'_>,
    ) -> Result<(), Report<wire::OperationError>> {
        require_process(context, &self.process_id)
    }

    async fn execute(
        self,
        context: InvocationCtx<'_>,
    ) -> Result<Self::Output, Report<wire::OperationError>> {
        context
            .state()
            .processes()
            .write_terminal(self)
            .await
            .map_err(process_error)
    }
}

#[async_trait]
impl ExecuteAction for api::ResizeProcessTerminalAction {
    fn check_permissions(
        &self,
        context: &InvocationCtx<'_>,
    ) -> Result<(), Report<wire::OperationError>> {
        require_process(context, &self.process_id)
    }

    async fn execute(
        self,
        context: InvocationCtx<'_>,
    ) -> Result<Self::Output, Report<wire::OperationError>> {
        context
            .state()
            .processes()
            .resize_terminal(self)
            .await
            .map_err(process_error)
    }
}

#[async_trait]
impl ExecuteAction for api::RemoveProcessAction {
    fn check_permissions(
        &self,
        context: &InvocationCtx<'_>,
    ) -> Result<(), Report<wire::OperationError>> {
        require_process(context, &self.process_id)
    }

    async fn execute(
        self,
        context: InvocationCtx<'_>,
    ) -> Result<Self::Output, Report<wire::OperationError>> {
        context
            .state()
            .processes()
            .remove(self)
            .map_err(process_error)
    }
}

#[async_trait]
impl ExecuteAction for api::SnapshotProcessTerminalAction {
    fn check_permissions(
        &self,
        context: &InvocationCtx<'_>,
    ) -> Result<(), Report<wire::OperationError>> {
        require_process(context, &self.process_id)
    }

    async fn execute(
        self,
        context: InvocationCtx<'_>,
    ) -> Result<Self::Output, Report<wire::OperationError>> {
        context
            .state()
            .processes()
            .snapshot_terminal(self)
            .map_err(process_error)
    }
}

#[async_trait]
impl OpenSubscription for api::ProcessLogSubscription {
    fn check_permissions(
        &self,
        context: &SubscriptionCtx<'_>,
    ) -> Result<(), Report<wire::OperationError>> {
        require_process_subscription(context, &self.process_id)
    }

    type Source = LogSubscription;

    async fn open(
        self,
        context: SubscriptionCtx<'_>,
    ) -> Result<Self::Source, Report<wire::OperationError>> {
        context
            .state()
            .processes()
            .subscribe_log(self)
            .map_err(process_error)
    }
}

#[async_trait]
impl EventSource for LogSubscription {
    type Event = api::ProcessLogEvent;

    async fn recv(&mut self) -> Result<Option<Self::Event>, Report<wire::OperationError>> {
        Ok(LogSubscription::recv(self).await)
    }
}

#[async_trait]
impl OpenSubscription for api::ProcessTerminalSubscription {
    fn check_permissions(
        &self,
        context: &SubscriptionCtx<'_>,
    ) -> Result<(), Report<wire::OperationError>> {
        require_process_subscription(context, &self.process_id)
    }

    type Source = TerminalSubscription;

    async fn open(
        self,
        context: SubscriptionCtx<'_>,
    ) -> Result<Self::Source, Report<wire::OperationError>> {
        context
            .state()
            .processes()
            .subscribe_terminal(self)
            .map_err(process_error)
    }
}

#[async_trait]
impl EventSource for TerminalSubscription {
    type Event = api::ProcessTerminalEvent;

    async fn recv(&mut self) -> Result<Option<Self::Event>, Report<wire::OperationError>> {
        Ok(TerminalSubscription::recv(self).await)
    }
}

#[async_trait]
impl OpenSubscription for api::ProcessListChangedSubscription {
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

    type Source = ProcessListSubscription;

    async fn open(
        self,
        context: SubscriptionCtx<'_>,
    ) -> Result<Self::Source, Report<wire::OperationError>> {
        context
            .state()
            .processes()
            .subscribe_process_list(self)
            .map_err(process_error)
    }
}

#[async_trait]
impl EventSource for ProcessListSubscription {
    type Event = api::ProcessListChangedEvent;

    async fn recv(&mut self) -> Result<Option<Self::Event>, Report<wire::OperationError>> {
        Ok(tascarrel_store::Subscription::recv(self)
            .await
            .map(|event| api::ProcessListChangedEvent {
                change: store_event(event),
            }))
    }
}

/// Returns the authenticated pod actor when the request originated in a pod.
fn caller_pod(context: &wire::RequestContext) -> Option<&wire::PodAddress> {
    match &context.caller {
        wire::Actor::Pod(address) => Some(address),
        _ => None,
    }
}

/// Authorizes an action whose input selects one pod.
fn require_pod_input(
    context: &InvocationCtx<'_>,
    pod_id: &tascarrel_api::types::pods::PodId,
) -> Result<(), Report<wire::OperationError>> {
    let routing = context.require_routing_context()?;
    if routing.caller.is_host_or_client() {
        return Ok(());
    }
    match caller_pod(routing) {
        Some(address) if address == context.target_pod()? && address.pod_id == *pod_id => Ok(()),
        _ => Err(wire::OperationError::forbidden()),
    }
}

/// Authorizes an action against a process owned by the authenticated pod.
fn require_process(
    context: &InvocationCtx<'_>,
    process_id: &api::ProcessId,
) -> Result<(), Report<wire::OperationError>> {
    let routing = context.require_routing_context()?;
    if routing.caller.is_host_or_client() {
        return Ok(());
    }
    let Some(address) = caller_pod(routing) else {
        return Err(wire::OperationError::forbidden());
    };
    if address == context.target_pod()?
        && context
            .state()
            .processes()
            .process_pod_id(process_id)
            .as_ref()
            == Some(&address.pod_id)
    {
        Ok(())
    } else {
        Err(wire::OperationError::forbidden())
    }
}

/// Authorizes a subscription to a process owned by the authenticated pod.
fn require_process_subscription(
    context: &SubscriptionCtx<'_>,
    process_id: &api::ProcessId,
) -> Result<(), Report<wire::OperationError>> {
    let routing = context.require_routing_context()?;
    if routing.caller.is_host_or_client() {
        return Ok(());
    }
    let Some(address) = caller_pod(routing) else {
        return Err(wire::OperationError::forbidden());
    };
    if address == context.target_pod()?
        && context
            .state()
            .processes()
            .process_pod_id(process_id)
            .as_ref()
            == Some(&address.pod_id)
    {
        Ok(())
    } else {
        Err(wire::OperationError::forbidden())
    }
}

/// Maps a process service report to a peer-visible contract error.
fn process_error(report: Report<ProcessSupervisorError>) -> Report<wire::OperationError> {
    let error = match report.error() {
        ProcessSupervisorError::InvalidRequest(message) => {
            wire::OperationError::InvalidRequest(operation_error_details(message.clone()))
        }
        ProcessSupervisorError::Internal(message) => {
            wire::OperationError::Internal(operation_error_details(message.clone()))
        }
    };
    report.escalate(error)
}
