//! Control-plane implementation for host-owned workspace configuration.

use async_trait::async_trait;
use reportify::Report;
use tascarrel_api::types::config as api;
use tascarrel_api::types::protocol as wire;

use crate::control_plane::SubscriptionCtx;
use crate::control_plane::operation_error_details;
use crate::control_plane::operations::EventSource;
use crate::control_plane::operations::ExecuteAction;
use crate::control_plane::operations::OpenSubscription;
use crate::services::config::ConfigServiceError;
use crate::services::config::ConfigSubscription;

#[async_trait]
impl ExecuteAction for api::UpdateWorkspaceSettingsAction {
    async fn check_permissions(
        &self,
        context: &crate::control_plane::InvocationCtx<'_>,
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
        context: crate::control_plane::InvocationCtx<'_>,
    ) -> Result<Self::Output, Report<wire::OperationError>> {
        context
            .state()
            .config()
            .update_settings(self)
            .await
            .map_err(config_error)
    }
}

#[async_trait]
impl OpenSubscription for api::ConfigChangedSubscription {
    async fn check_permissions(
        &self,
        context: &SubscriptionCtx<'_>,
    ) -> Result<(), Report<wire::OperationError>> {
        let caller = &context.require_routing_context()?.caller;
        if caller_may_read_workspace_config(caller, &self.workspace_name) {
            Ok(())
        } else {
            Err(wire::OperationError::forbidden())
        }
    }

    type Source = ConfigSubscription;

    async fn open(
        self,
        context: SubscriptionCtx<'_>,
    ) -> Result<Self::Source, Report<wire::OperationError>> {
        context
            .state()
            .config()
            .subscribe(self)
            .await
            .map_err(config_error)
    }
}

#[async_trait]
impl EventSource for ConfigSubscription {
    type Event = api::ConfigChangedEvent;

    async fn recv(&mut self) -> Result<Option<Self::Event>, Report<wire::OperationError>> {
        Ok(ConfigSubscription::recv(self).await)
    }
}

/// Maps a configuration service report to a peer-visible operation error.
fn config_error(report: Report<ConfigServiceError>) -> Report<wire::OperationError> {
    let details = operation_error_details(report.to_string());
    let error = match report.error() {
        ConfigServiceError::InvalidRequest => wire::OperationError::InvalidRequest(details),
        ConfigServiceError::Unavailable => wire::OperationError::Unavailable(details),
        ConfigServiceError::InvalidConfiguration
        | ConfigServiceError::RuntimeUnavailable
        | ConfigServiceError::Internal => wire::OperationError::Internal(details),
    };
    report.escalate(error)
}

/// Returns whether an authenticated caller may observe one workspace's host
/// configuration.
fn caller_may_read_workspace_config(
    caller: &wire::Actor,
    workspace_name: &tascarrel_api::types::workspaces::WorkspaceName,
) -> bool {
    caller.is_host_or_client()
        || matches!(
            caller,
            wire::Actor::Workspace(address) if &address.workspace == workspace_name
        )
}

#[cfg(test)]
mod tests {
    use tascarrel_api::types::pods::PodId;
    use tascarrel_api::types::workspaces::WorkspaceName;

    use super::*;

    /// Verifies workspace daemons can observe only their own host
    /// configuration, while hostd can observe every workspace.
    #[test]
    fn config_subscription_is_scoped_to_the_authenticated_workspace() {
        let alpha = WorkspaceName::new("alpha");
        let beta = WorkspaceName::new("beta");
        let alpha_workspace = wire::Actor::Workspace(wire::WorkspaceAddress {
            workspace: alpha.clone(),
        });
        let alpha_pod = wire::Actor::Pod(wire::PodAddress {
            workspace: alpha.clone(),
            pod_id: PodId::generate(),
        });

        assert!(caller_may_read_workspace_config(&wire::Actor::Host, &beta));
        assert!(caller_may_read_workspace_config(&alpha_workspace, &alpha));
        assert!(!caller_may_read_workspace_config(&alpha_workspace, &beta));
        assert!(!caller_may_read_workspace_config(&alpha_pod, &alpha));
    }
}
