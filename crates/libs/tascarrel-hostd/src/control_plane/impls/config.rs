//! Control-plane implementation for host-owned workspace configuration.

use async_trait::async_trait;
use reportify::ErrorExt as _;
use reportify::Report;
use tascarrel_api::ArcVec;
use tascarrel_api::types::chats;
use tascarrel_api::types::config as api;
use tascarrel_api::types::protocol as wire;
use tascarrel_api::types::secrets;

use crate::control_plane::SubscriptionCtx;
use crate::control_plane::operation_error_details;
use crate::control_plane::operations::EventSource;
use crate::control_plane::operations::ExecuteAction;
use crate::control_plane::operations::OpenSubscription;
use crate::services::config::ConfigServiceError;
use crate::services::config::ConfigSubscription;
use crate::services::secrets::SecretsServiceError;

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
impl ExecuteAction for api::ResolveTasciModelAction {
    async fn check_permissions(
        &self,
        context: &crate::control_plane::InvocationCtx<'_>,
    ) -> Result<(), Report<wire::OperationError>> {
        let caller = &context.require_routing_context()?.caller;
        if matches!(
            caller,
            wire::Actor::Workspace(address) if address.workspace == self.workspace_name
        ) {
            Ok(())
        } else {
            Err(wire::OperationError::forbidden())
        }
    }

    async fn execute(
        self,
        context: crate::control_plane::InvocationCtx<'_>,
    ) -> Result<Self::Output, Report<wire::OperationError>> {
        resolve_tasci_model(self, context).await
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

async fn resolve_tasci_model(
    input: api::ResolveTasciModelAction,
    context: crate::control_plane::InvocationCtx<'_>,
) -> Result<api::ResolveTasciModelOutput, Report<wire::OperationError>> {
    let snapshot = context
        .state()
        .config()
        .read(&input.workspace_name)
        .await
        .map_err(config_error)?;
    let tasci = snapshot
        .settings
        .as_ref()
        .and_then(|settings| settings.chat.as_ref())
        .and_then(|chat| chat.tasci.as_ref())
        .ok_or_else(|| invalid_tasci_request("Tasci is not configured for this workspace"))?;
    let models = tasci
        .models
        .as_ref()
        .filter(|models| !models.is_empty())
        .ok_or_else(|| invalid_tasci_request("Tasci has no configured models"))?;
    let endpoints = tasci
        .endpoints
        .as_ref()
        .ok_or_else(|| invalid_tasci_request("Tasci has no configured endpoints"))?;
    let default_model = tasci
        .default_model
        .as_ref()
        .map(ToString::to_string)
        .or_else(|| models.keys().next().map(ToString::to_string))
        .ok_or_else(|| invalid_tasci_request("Tasci has no default model"))?;
    let selected_model = input
        .model
        .as_ref()
        .map_or(default_model.as_str(), AsRef::as_ref);
    let model = models
        .get(selected_model)
        .ok_or_else(|| invalid_tasci_request("the selected Tasci model is not configured"))?;
    let endpoint = endpoints
        .get(model.endpoint.as_ref())
        .ok_or_else(|| invalid_tasci_request("the selected Tasci endpoint is not configured"))?;
    let (authorization_header, authorization_value) =
        if let Some(authorization) = endpoint.authorization.as_ref() {
            let revealed = context
                .state()
                .secrets()
                .reveal(
                    secrets::RevealSecretAction {
                        workspace_name: input.workspace_name.clone(),
                        provider_name: authorization.credential.provider.clone(),
                        secret_name: authorization.credential.secret.clone(),
                    },
                    context.state().config(),
                )
                .await
                .map_err(secret_error)?;
            (
                Some(authorization.header.clone()),
                Some(
                    format!(
                        "{}{}",
                        authorization.prefix.as_deref().unwrap_or_default(),
                        revealed.value
                    )
                    .into(),
                ),
            )
        } else {
            (None, None)
        };
    let catalog = models
        .iter()
        .map(|(alias, configured)| chats::ChatModel {
            id: alias.clone(),
            display_name: configured
                .display_name
                .clone()
                .unwrap_or_else(|| alias.clone()),
            short_name: None,
            is_custom: true,
            options: ArcVec::new(),
            pricing: configured.pricing.clone(),
        })
        .collect::<Vec<_>>()
        .into();
    Ok(api::ResolveTasciModelOutput {
        selected_model: selected_model.to_owned().into(),
        base_url: endpoint.base_url.clone(),
        provider_model: model.model.clone(),
        authorization_header,
        authorization_value,
        default_model: default_model.into(),
        models: catalog,
    })
}

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

fn invalid_tasci_request(message: &str) -> Report<wire::OperationError> {
    wire::OperationError::InvalidRequest(operation_error_details(message)).report()
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
