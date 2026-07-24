//! Typed host control-plane dispatch and service composition.
//!
//! [`HostState`] composes the services owned by the host daemon.
//! [`HostControlService`] terminates authenticated client links and persistent
//! workspace peer connections, applies topology policy, and dispatches typed
//! host operations.

mod impls;
mod operations;
mod service;

use reportify::ErrorExt as _;
use reportify::Report;
pub use service::HostControlService;
use tascarrel_api::types::protocol as wire;

use crate::services::config::ConfigService;
use crate::services::network::NetworkService;
use crate::services::repositories::RepositoryService;
use crate::services::secrets::SecretsService;
use crate::services::workspaces::WorkspaceService;

/// Services and state shared by every host control-plane connection.
#[derive(Clone)]
pub struct HostState {
    workspaces: WorkspaceService,
    config: ConfigService,
    network: NetworkService,
    repositories: RepositoryService,
    secrets: SecretsService,
}

impl HostState {
    /// Creates host state from its long-lived services.
    #[must_use]
    pub fn new(
        workspaces: WorkspaceService,
        config: ConfigService,
        network: NetworkService,
        repositories: RepositoryService,
        secrets: SecretsService,
    ) -> Self {
        Self {
            workspaces,
            config,
            network,
            repositories,
            secrets,
        }
    }

    /// Returns the workspace lifecycle service.
    pub(crate) fn workspaces(&self) -> &WorkspaceService {
        &self.workspaces
    }

    /// Returns the workspace configuration service.
    pub(crate) fn config(&self) -> &ConfigService {
        &self.config
    }

    /// Returns the host-owned network service.
    pub(crate) fn network(&self) -> &NetworkService {
        &self.network
    }

    /// Returns the host-owned configured repository inventory service.
    pub(crate) fn repositories(&self) -> &RepositoryService {
        &self.repositories
    }

    /// Returns the host-owned secret-provider service.
    pub(crate) fn secrets(&self) -> &SecretsService {
        &self.secrets
    }
}

/// State and complete wire request available to one typed host action.
pub(crate) struct InvocationCtx<'a> {
    state: &'a HostState,
    invocation: &'a wire::RpcInvocation,
}

impl<'a> InvocationCtx<'a> {
    /// Creates a context for one decoded action request.
    pub(crate) const fn new(state: &'a HostState, invocation: &'a wire::RpcInvocation) -> Self {
        Self { state, invocation }
    }

    /// Returns the host services available to the action.
    pub(crate) const fn state(&self) -> &'a HostState {
        self.state
    }

    /// Requires the authenticated routing context attached to the action.
    pub(crate) fn require_routing_context(
        &self,
    ) -> Result<&'a wire::RequestContext, Report<wire::OperationError>> {
        self.invocation
            .context
            .as_ref()
            .ok_or_else(|| invalid_request("validated host operation has no routing context"))
    }
}

/// State and complete wire request available to one typed host subscription.
pub(crate) struct SubscriptionCtx<'a> {
    state: &'a HostState,
    subscription: &'a wire::SubscriptionStart,
}

impl<'a> SubscriptionCtx<'a> {
    /// Creates a context for one decoded subscription request.
    pub(crate) const fn new(
        state: &'a HostState,
        subscription: &'a wire::SubscriptionStart,
    ) -> Self {
        Self {
            state,
            subscription,
        }
    }

    /// Returns the host services available to the subscription.
    pub(crate) const fn state(&self) -> &'a HostState {
        self.state
    }

    /// Requires the authenticated routing context attached to the subscription.
    pub(crate) fn require_routing_context(
        &self,
    ) -> Result<&'a wire::RequestContext, Report<wire::OperationError>> {
        self.subscription
            .context
            .as_ref()
            .ok_or_else(|| invalid_request("validated host operation has no routing context"))
    }
}

/// Creates a contract operation error.
pub(crate) fn invalid_request(message: impl Into<String>) -> Report<wire::OperationError> {
    wire::OperationError::InvalidRequest(operation_error_details(message)).report()
}

/// Creates operation details for a standalone diagnostic message.
pub(crate) fn operation_error_details(message: impl Into<String>) -> wire::OperationErrorDetails {
    wire::OperationErrorDetails {
        message: message.into().into(),
        report: None,
    }
}
