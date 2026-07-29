//! Synchronization of host-owned Tasci settings into guest harness state.
//!
//! [`synchronize`] follows workspace configuration updates, resolves the
//! canonical catalog through hostd, and publishes it through guestd's harness
//! manager.

use reportify::Report;
use tascarrel_api::types::config;
use tascarrel_api::types::protocol as wire;
use tascarrel_api::types::workspaces::WorkspaceName;

use super::GuestState;
use super::HostClient;
use super::HostClientError;

/// Mirrors the host-resolved Tasci catalog into ordinary guest harness state.
#[tracing::instrument(level = "debug", skip(state, host), fields(workspace = %workspace))]
pub(crate) async fn synchronize(
    state: &GuestState,
    host: &HostClient,
    workspace: &WorkspaceName,
) -> Result<(), Report<HostClientError>> {
    let mut changes = host
        .subscribe(
            workspace_request_context(workspace),
            config::ConfigChangedSubscription {
                workspace_name: workspace.clone(),
            },
        )
        .await?;
    while let Some(event) = changes.recv().await? {
        let harnesses = state.chats().harnesses();
        if !catalog_is_configured(&event) {
            harnesses.clear_tasci_catalog();
            continue;
        }
        match host
            .execute(
                workspace_request_context(workspace),
                config::ResolveTasciModelAction {
                    workspace_name: workspace.clone(),
                    model: None,
                },
            )
            .await
        {
            Ok(output) => harnesses.configure_tasci(output),
            Err(error)
                if matches!(
                    error.error(),
                    HostClientError::Remote(wire::OperationError::InvalidRequest(_))
                ) =>
            {
                harnesses.clear_tasci_catalog();
                tracing::warn!(
                    error = %error,
                    "workspace settings do not define a valid Tasci model catalog"
                );
            }
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    "could not refresh the Tasci model catalog from host settings"
                );
            }
        }
    }
    Ok(())
}

/// Returns whether settings contain a Tasci catalog worth resolving.
fn catalog_is_configured(event: &config::ConfigChangedEvent) -> bool {
    event
        .settings
        .as_ref()
        .and_then(|settings| settings.chat.as_ref())
        .and_then(|chat| chat.tasci.as_ref())
        .and_then(|tasci| tasci.models.as_ref())
        .is_some_and(|models| !models.is_empty())
}

/// Creates a workspace-authenticated context for background host operations.
fn workspace_request_context(workspace: &WorkspaceName) -> wire::RequestContext {
    let actor = wire::Actor::Workspace(wire::WorkspaceAddress {
        workspace: workspace.clone(),
    });
    wire::RequestContext {
        origin: actor.clone(),
        caller: actor,
        trace_id: wire::TraceId::generate(),
        caused_by: None,
    }
}
