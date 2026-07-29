//! Control-plane implementation for host-owned workspace configuration.

use async_trait::async_trait;
use reportify::ErrorExt as _;
use reportify::Report;
use tascarrel_api::ArcVec;
use tascarrel_api::types::chats;
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
            (
                Some(authorization.header.clone()),
                Some(tasci_authorization_value(authorization)?),
            )
        } else {
            (None, None)
        };
    let catalog = models
        .iter()
        .map(
            |(alias, configured)| -> Result<_, Report<wire::OperationError>> {
                let configured_endpoint =
                    endpoints.get(configured.endpoint.as_ref()).ok_or_else(|| {
                        invalid_tasci_request(
                            "a configured Tasci model refers to an unknown endpoint",
                        )
                    })?;
                let model_name = configured.display_name.as_deref().unwrap_or(alias.as_ref());
                let provider_name = configured_endpoint
                    .display_name
                    .as_deref()
                    .unwrap_or(configured.endpoint.as_ref());
                Ok(chats::ChatModel {
                    id: alias.clone(),
                    display_name: format!("{model_name} ({provider_name})").into(),
                    short_name: None,
                    is_custom: true,
                    options: ArcVec::new(),
                    pricing: configured.pricing.clone(),
                })
            },
        )
        .collect::<Result<Vec<_>, _>>()?
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

/// Resolves only non-secret authorization metadata for the Tasci process.
fn tasci_authorization_value(
    authorization: &api::WorkspaceTasciAuthorization,
) -> Result<tascarrel_api::ArcStr, Report<wire::OperationError>> {
    if let Some(value) = &authorization.value {
        return Ok(value.clone());
    }
    let credential = authorization.credential.as_ref().ok_or_else(|| {
        invalid_tasci_request("the selected Tasci endpoint has invalid authorization settings")
    })?;
    let placeholder = format!(
        "tascarrel-secret:{}",
        credential.secret.to_ascii_lowercase().replace('_', "-")
    );
    Ok(format!(
        "{}{placeholder}",
        authorization.prefix.as_deref().unwrap_or_default()
    )
    .into())
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

    /// Verifies legacy Tasci secret references become non-secret network
    /// placeholders instead of resolving a credential.
    #[test]
    fn legacy_tasci_authorization_uses_the_default_network_placeholder() {
        let authorization = api::WorkspaceTasciAuthorization {
            header: "Authorization".into(),
            value: None,
            prefix: Some("Bearer ".into()),
            credential: Some(api::WorkspaceSecretReference {
                provider: "project".into(),
                secret: "API_TOKEN".into(),
            }),
        };

        assert_eq!(
            tasci_authorization_value(&authorization).unwrap().as_ref(),
            "Bearer tascarrel-secret:api-token"
        );
    }
}
