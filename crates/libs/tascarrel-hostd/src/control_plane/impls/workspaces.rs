//! Control-plane implementations for host-owned workspace lifecycle and logs.

use async_trait::async_trait;
use reportify::Report;
use tascarrel_api::types::protocol as wire;
use tascarrel_api::types::workspaces as api;

use crate::control_plane::InvocationCtx;
use crate::control_plane::SubscriptionCtx;
use crate::control_plane::operation_error_details;
use crate::control_plane::operations::EventSource;
use crate::control_plane::operations::ExecuteAction;
use crate::control_plane::operations::OpenSubscription;
use crate::control_plane::operations::store_event;
use crate::services::secrets::SecretsServiceError;
use crate::services::workspaces::UsbDeviceSubscription;
use crate::services::workspaces::WorkspaceListSubscription;
use crate::services::workspaces::WorkspaceServiceError;
use crate::services::workspaces::WorkspaceVmLogSubscription;

macro_rules! implement_action {
    ($action:ty, $method:ident) => {
        #[async_trait]
        impl ExecuteAction for $action {
            async fn check_permissions(
                &self,
                context: &InvocationCtx<'_>,
            ) -> Result<(), Report<wire::OperationError>> {
                require_host_or_client(&context.require_routing_context()?.caller)
            }

            async fn execute(
                self,
                context: InvocationCtx<'_>,
            ) -> Result<Self::Output, Report<wire::OperationError>> {
                context
                    .state()
                    .workspaces()
                    .$method(self)
                    .await
                    .map_err(workspace_error)
            }
        }
    };
}

implement_action!(api::StartWorkspaceAction, start);
implement_action!(api::StopWorkspaceAction, stop);
implement_action!(api::AttachUsbDeviceAction, attach_usb_device);
implement_action!(api::DetachUsbDeviceAction, detach_usb_device);

#[async_trait]
impl ExecuteAction for api::CreateWorkspaceAction {
    async fn check_permissions(
        &self,
        context: &InvocationCtx<'_>,
    ) -> Result<(), Report<wire::OperationError>> {
        require_host_or_client(&context.require_routing_context()?.caller)
    }

    #[tracing::instrument(level = "debug", skip_all)]
    async fn execute(
        self,
        context: InvocationCtx<'_>,
    ) -> Result<Self::Output, Report<wire::OperationError>> {
        let initial_secrets = self.initial_secrets.clone().unwrap_or_default();
        let prepared = context
            .state()
            .workspaces()
            .prepare_create(self)
            .await
            .map_err(workspace_error)?;
        context
            .state()
            .secrets()
            .initialize_workspace_secrets(prepared.staging_directory(), &initial_secrets)
            .await
            .map_err(workspace_secret_error)?;
        context
            .state()
            .workspaces()
            .publish_create(prepared)
            .await
            .map_err(workspace_error)
    }
}

#[async_trait]
impl ExecuteAction for api::DestroyWorkspaceAction {
    async fn check_permissions(
        &self,
        context: &InvocationCtx<'_>,
    ) -> Result<(), Report<wire::OperationError>> {
        require_host_or_client(&context.require_routing_context()?.caller)
    }

    async fn execute(
        self,
        context: InvocationCtx<'_>,
    ) -> Result<Self::Output, Report<wire::OperationError>> {
        let workspace = self.workspace.clone();
        let output = context
            .state()
            .workspaces()
            .destroy(self)
            .await
            .map_err(workspace_error)?;
        context.state().network().remove_workspace(&workspace).await;
        Ok(output)
    }
}

#[async_trait]
impl OpenSubscription for api::WorkspaceListChangedSubscription {
    async fn check_permissions(
        &self,
        context: &SubscriptionCtx<'_>,
    ) -> Result<(), Report<wire::OperationError>> {
        require_host_or_client(&context.require_routing_context()?.caller)
    }

    type Source = WorkspaceListSubscription;

    async fn open(
        self,
        context: SubscriptionCtx<'_>,
    ) -> Result<Self::Source, Report<wire::OperationError>> {
        context
            .state()
            .workspaces()
            .subscribe(&self)
            .map_err(workspace_error)
    }
}

#[async_trait]
impl EventSource for WorkspaceListSubscription {
    type Event = api::WorkspaceListChangedEvent;

    async fn recv(&mut self) -> Result<Option<Self::Event>, Report<wire::OperationError>> {
        Ok(tascarrel_store::Subscription::recv(self)
            .await
            .map(|change| api::WorkspaceListChangedEvent {
                change: store_event(change),
            }))
    }
}

#[async_trait]
impl OpenSubscription for api::WorkspaceVmLogSubscription {
    async fn check_permissions(
        &self,
        context: &SubscriptionCtx<'_>,
    ) -> Result<(), Report<wire::OperationError>> {
        require_host_or_client(&context.require_routing_context()?.caller)
    }

    type Source = WorkspaceVmLogSubscription;

    async fn open(
        self,
        context: SubscriptionCtx<'_>,
    ) -> Result<Self::Source, Report<wire::OperationError>> {
        context
            .state()
            .workspaces()
            .subscribe_vm_log(&self)
            .map_err(workspace_error)
    }
}

#[async_trait]
impl EventSource for WorkspaceVmLogSubscription {
    type Event = api::WorkspaceVmLogEvent;

    async fn recv(&mut self) -> Result<Option<Self::Event>, Report<wire::OperationError>> {
        Ok(WorkspaceVmLogSubscription::recv(self).await)
    }
}

#[async_trait]
impl OpenSubscription for api::UsbDevicesChangedSubscription {
    async fn check_permissions(
        &self,
        context: &SubscriptionCtx<'_>,
    ) -> Result<(), Report<wire::OperationError>> {
        require_host_or_client(&context.require_routing_context()?.caller)
    }

    type Source = UsbDeviceSubscription;

    async fn open(
        self,
        context: SubscriptionCtx<'_>,
    ) -> Result<Self::Source, Report<wire::OperationError>> {
        context
            .state()
            .workspaces()
            .subscribe_usb_devices(&self)
            .map_err(workspace_error)
    }
}

#[async_trait]
impl EventSource for UsbDeviceSubscription {
    type Event = api::UsbDevicesChangedEvent;

    async fn recv(&mut self) -> Result<Option<Self::Event>, Report<wire::OperationError>> {
        Ok(UsbDeviceSubscription::recv(self).await)
    }
}

fn require_host_or_client(caller: &wire::Actor) -> Result<(), Report<wire::OperationError>> {
    if caller.is_host_or_client() {
        Ok(())
    } else {
        Err(wire::OperationError::forbidden())
    }
}

fn workspace_error(report: Report<WorkspaceServiceError>) -> Report<wire::OperationError> {
    let details = operation_error_details(report.to_string());
    let error = match report.error() {
        WorkspaceServiceError::InvalidRequest(_) => wire::OperationError::InvalidRequest(details),
        WorkspaceServiceError::Unavailable(_) => wire::OperationError::Unavailable(details),
        WorkspaceServiceError::Internal(_) => wire::OperationError::Internal(details),
    };
    report.escalate(error)
}

fn workspace_secret_error(report: Report<SecretsServiceError>) -> Report<wire::OperationError> {
    let details = operation_error_details(report.to_string());
    let error = match report.error() {
        SecretsServiceError::InvalidRequest => wire::OperationError::InvalidRequest(details),
        SecretsServiceError::Unavailable => wire::OperationError::Unavailable(details),
        SecretsServiceError::InvalidConfiguration | SecretsServiceError::Internal => {
            wire::OperationError::Internal(details)
        }
    };
    report.escalate(error)
}
