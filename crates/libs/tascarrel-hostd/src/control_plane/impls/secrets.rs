//! Control-plane implementation for host-owned secret providers.

use async_trait::async_trait;
use reportify::Report;
use tascarrel_api::types::protocol as wire;
use tascarrel_api::types::secrets as api;

use crate::control_plane::InvocationCtx;
use crate::control_plane::SubscriptionCtx;
use crate::control_plane::operation_error_details;
use crate::control_plane::operations::EventSource;
use crate::control_plane::operations::ExecuteAction;
use crate::control_plane::operations::OpenSubscription;
use crate::services::secrets::SecretsServiceError;
use crate::services::secrets::SecretsSubscription;

macro_rules! implement_secret_action {
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
                    .secrets()
                    .$method(self, context.state().config())
                    .await
                    .map_err(secret_error)
            }
        }
    };
}

implement_secret_action!(api::RevealSecretAction, reveal);
implement_secret_action!(api::SetSecretAction, set);
implement_secret_action!(api::DeleteSecretAction, delete);

#[async_trait]
impl OpenSubscription for api::SecretsChangedSubscription {
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

    type Source = SecretsSubscription;

    async fn open(
        self,
        context: SubscriptionCtx<'_>,
    ) -> Result<Self::Source, Report<wire::OperationError>> {
        context
            .state()
            .secrets()
            .subscribe(self, context.state().config())
            .await
            .map_err(secret_error)
    }
}

#[async_trait]
impl EventSource for SecretsSubscription {
    type Event = api::SecretsChangedEvent;

    async fn recv(&mut self) -> Result<Option<Self::Event>, Report<wire::OperationError>> {
        Ok(SecretsSubscription::recv(self).await)
    }
}

/// Maps secret service categories onto the shared control-plane contract.
fn secret_error(report: Report<SecretsServiceError>) -> Report<wire::OperationError> {
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
