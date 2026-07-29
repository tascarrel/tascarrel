//! Control-plane implementations for the pod lifecycle schema.

use async_trait::async_trait;
use reportify::Report;
use tascarrel_api::types::pods as api;
use tascarrel_api::types::protocol as wire;

use crate::control_plane::InvocationCtx;
use crate::control_plane::SubscriptionCtx;
use crate::control_plane::operation_error_details;
use crate::control_plane::operations::EventSource;
use crate::control_plane::operations::ExecuteAction;
use crate::control_plane::operations::OpenSubscription;
use crate::control_plane::operations::store_event;
use crate::repositories::RepositoryImportError;
use crate::services::pods::PodListSubscription;
use crate::services::pods::PodServiceError;

#[async_trait]
impl ExecuteAction for api::CreatePodAction {
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
        let repository_preparation = context.repository_preparation_task()?;
        context
            .state()
            .pods()
            .create_with_repository_preparation_task(
                self,
                context.state().images(),
                context.state().processes(),
                std::sync::Arc::clone(context.state().network()),
                std::sync::Arc::clone(context.state().image_input()),
                async move {
                    repository_preparation
                        .await
                        .map_err(pod_repository_preparation_error)
                },
            )
            .map_err(pod_error)
    }
}

#[async_trait]
impl ExecuteAction for api::StartPodAction {
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
        context
            .state()
            .pods()
            .start(self, context.state().processes(), context.state().network())
            .await
            .map_err(pod_error)
    }
}

#[async_trait]
impl ExecuteAction for api::StopPodAction {
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
        context
            .state()
            .pods()
            .stop(self, context.state().network())
            .await
            .map_err(pod_error)
    }
}

#[async_trait]
impl ExecuteAction for api::DestroyPodAction {
    fn check_permissions(
        &self,
        context: &InvocationCtx<'_>,
    ) -> Result<(), Report<wire::OperationError>> {
        if context
            .require_routing_context()?
            .caller
            .is_host_or_client()
            || caller_is_target_pod(context, &self.pod_id)?
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
        if caller_is_target_pod(&context, &self.pod_id)? {
            let pods = context.state().pods().clone();
            let chats = context.state().chats().clone();
            let network = std::sync::Arc::clone(context.state().network());
            let pod_id = self.pod_id.clone();
            // Awaiting cleanup would deadlock when the active chat harness
            // invokes this RPC: archival waits for the harness to exit while
            // the harness waits for this response.
            tokio::spawn(async move {
                if let Err(error) = pods.destroy(self, &chats, &network).await {
                    tracing::error!(pod_id = %pod_id.0, %error, "failed to destroy pod");
                }
            });
            return Ok(api::DestroyPodOutput {});
        }
        context
            .state()
            .pods()
            .destroy(self, context.state().chats(), context.state().network())
            .await
            .map_err(pod_error)
    }
}

#[async_trait]
impl ExecuteAction for api::SetPodTitleAction {
    fn check_permissions(
        &self,
        context: &InvocationCtx<'_>,
    ) -> Result<(), Report<wire::OperationError>> {
        let caller = &context.require_routing_context()?.caller;
        if caller.is_host_or_client() || caller_is_target_pod(context, &self.pod_id)? {
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
            .pods()
            .set_title(&self.pod_id, self.title)
            .await
            .map_err(pod_error)?;
        Ok(api::SetPodTitleOutput {})
    }
}

#[async_trait]
impl ExecuteAction for api::ImportPodRepositoryAction {
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
        let preparation = context.repository_preparation().await?.ok_or_else(|| {
            Report::new(wire::OperationError::InvalidRequest(
                operation_error_details("workspace repository imports are unavailable"),
            ))
        })?;
        let result = preparation
            .import_into_running_pod(
                &self.pod_id,
                self.path.as_ref(),
                context.state().pods(),
                context.state().processes(),
            )
            .await
            .map_err(repository_import_error)?;
        if let Err(error) = context
            .state()
            .changes()
            .refresh_inventory(
                context.state().pods().clone(),
                context.state().repositories().cloned(),
                context.state().repository_config().cloned(),
            )
            .await
        {
            tracing::warn!(
                pod_id = %self.pod_id.0,
                path = %self.path,
                %error,
                "could not refresh repository status after pod import"
            );
        }
        Ok(api::ImportPodRepositoryOutput { result })
    }
}

#[async_trait]
impl OpenSubscription for api::PodListChangedSubscription {
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

    type Source = PodListSubscription;

    async fn open(
        self,
        context: SubscriptionCtx<'_>,
    ) -> Result<Self::Source, Report<wire::OperationError>> {
        context.state().pods().subscribe(&self).map_err(pod_error)
    }
}

#[async_trait]
impl EventSource for PodListSubscription {
    type Event = api::PodListChangedEvent;

    async fn recv(&mut self) -> Result<Option<Self::Event>, Report<wire::OperationError>> {
        Ok(tascarrel_store::Subscription::recv(self)
            .await
            .map(|event| api::PodListChangedEvent {
                change: store_event(event),
            }))
    }
}

/// Maps a pod service report to a peer-visible operation error.
fn pod_error(report: Report<PodServiceError>) -> Report<wire::OperationError> {
    let error = match report.error() {
        PodServiceError::InvalidRequest(message) => {
            wire::OperationError::InvalidRequest(operation_error_details(message.clone()))
        }
        PodServiceError::Internal(message) => {
            wire::OperationError::Internal(operation_error_details(message.clone()))
        }
    };
    report.escalate(error)
}

/// Maps a repository import report to its control-plane error category.
fn repository_import_error(report: Report<RepositoryImportError>) -> Report<wire::OperationError> {
    let error = match report.error() {
        RepositoryImportError::InvalidRequest(message) => {
            wire::OperationError::InvalidRequest(operation_error_details(message.clone()))
        }
        RepositoryImportError::Internal(message) => {
            wire::OperationError::Internal(operation_error_details(message.clone()))
        }
    };
    report.escalate(error)
}

/// Preserves repository preparation diagnostics under the pod service error
/// category.
fn pod_repository_preparation_error(
    error: Report<wire::OperationError>,
) -> Report<PodServiceError> {
    error.escalate(PodServiceError::Internal(
        "failed to prepare pod repositories".to_owned(),
    ))
}

/// Returns whether the authenticated caller owns the addressed pod operation.
fn caller_is_target_pod(
    context: &InvocationCtx<'_>,
    pod_id: &api::PodId,
) -> Result<bool, Report<wire::OperationError>> {
    Ok(matches!(
        &context.require_routing_context()?.caller,
        wire::Actor::Pod(address)
            if address == context.target_pod()? && address.pod_id == *pod_id
    ))
}
