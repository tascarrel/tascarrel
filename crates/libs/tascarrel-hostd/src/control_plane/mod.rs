//! Typed host control-plane dispatch and service composition.
//!
//! [`HostState`] composes the services owned by the host daemon.
//! [`HostControlService`] terminates authenticated client links and persistent
//! workspace peer connections, applies topology policy, and dispatches typed
//! host operations.

mod bootstrap;
mod impls;
mod operations;
mod service;

pub(crate) use bootstrap::BootstrapControlService;
use reportify::ErrorExt as _;
use reportify::Report;
pub use service::HostControlService;
use tascarrel_api::types::protocol as wire;

use crate::services::auth::AuthService;
use crate::services::auth::AuthServiceError;
use crate::services::automations::AutomationService;
use crate::services::config::ConfigService;
use crate::services::host_operations::HostOperationService;
use crate::services::network::NetworkService;
use crate::services::repositories::RepositoryService;
use crate::services::secrets::SecretsService;
use crate::services::share_overlays::ShareOverlayService;
use crate::services::workspaces::WorkspaceService;

/// Services and state shared by every host control-plane connection.
#[derive(Clone)]
pub struct HostState {
    auth: AuthService,
    automations: AutomationService,
    workspaces: WorkspaceService,
    config: ConfigService,
    host_operations: HostOperationService,
    network: NetworkService,
    repositories: RepositoryService,
    secrets: SecretsService,
    share_overlays: ShareOverlayService,
}

impl HostState {
    /// Creates host state from its long-lived services.
    #[must_use]
    #[allow(
        clippy::too_many_arguments,
        reason = "the host state is the explicit composition root for independent services"
    )]
    pub fn new(
        auth: AuthService,
        automations: AutomationService,
        workspaces: WorkspaceService,
        config: ConfigService,
        host_operations: HostOperationService,
        network: NetworkService,
        repositories: RepositoryService,
        secrets: SecretsService,
        share_overlays: ShareOverlayService,
    ) -> Self {
        Self {
            auth,
            automations,
            workspaces,
            config,
            host_operations,
            network,
            repositories,
            secrets,
            share_overlays,
        }
    }

    /// Returns the host-owned browser authentication service.
    pub(crate) fn auth(&self) -> &AuthService {
        &self.auth
    }

    /// Returns the durable workspace Automation service.
    pub(crate) fn automations(&self) -> &AutomationService {
        &self.automations
    }

    /// Returns the workspace lifecycle service.
    pub(crate) fn workspaces(&self) -> &WorkspaceService {
        &self.workspaces
    }

    /// Returns the workspace configuration service.
    pub(crate) fn config(&self) -> &ConfigService {
        &self.config
    }

    /// Returns the durable approval-gated host process service.
    pub(crate) fn host_operations(&self) -> &HostOperationService {
        &self.host_operations
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

    /// Returns the durable overlay-share approval service.
    pub(crate) fn share_overlays(&self) -> &ShareOverlayService {
        &self.share_overlays
    }
}

/// Authority established by the transport terminating at hostd.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ConnectionPrincipal {
    /// A process connected through hostd's private Unix socket.
    LocalAdmin,
    /// A browser authenticated by a durable host session cookie.
    Browser {
        /// Durable browser session established by the host cookie.
        session_id: tascarrel_api::types::auth::BrowserSessionId,
        /// Exact origin on which the browser paired.
        origin: String,
    },
    /// An authenticated internal host or guest connection.
    Internal,
}

/// State and complete wire request available to one typed host action.
pub(crate) struct InvocationCtx<'a> {
    state: &'a HostState,
    invocation: &'a wire::RpcInvocation,
    principal: &'a ConnectionPrincipal,
}

impl<'a> InvocationCtx<'a> {
    /// Creates a context for one decoded action request.
    pub(crate) const fn new(
        state: &'a HostState,
        invocation: &'a wire::RpcInvocation,
        principal: &'a ConnectionPrincipal,
    ) -> Self {
        Self {
            state,
            invocation,
            principal,
        }
    }

    /// Returns the host services available to the action.
    pub(crate) const fn state(&self) -> &'a HostState {
        self.state
    }

    /// Returns the authority established by this connection's transport.
    pub(crate) const fn principal(&self) -> &'a ConnectionPrincipal {
        self.principal
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
    principal: &'a ConnectionPrincipal,
}

impl<'a> SubscriptionCtx<'a> {
    /// Creates a context for one decoded subscription request.
    pub(crate) const fn new(
        state: &'a HostState,
        subscription: &'a wire::SubscriptionStart,
        principal: &'a ConnectionPrincipal,
    ) -> Self {
        Self {
            state,
            subscription,
            principal,
        }
    }

    /// Returns the host services available to the subscription.
    pub(crate) const fn state(&self) -> &'a HostState {
        self.state
    }

    /// Returns the authority established by this connection's transport.
    pub(crate) const fn principal(&self) -> &'a ConnectionPrincipal {
        self.principal
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

/// Maps authentication service categories onto the control-plane contract.
pub(crate) fn auth_operation_error(
    report: Report<AuthServiceError>,
) -> Report<wire::OperationError> {
    let details = operation_error_details(report.to_string());
    match report.error() {
        AuthServiceError::InvalidRequest
        | AuthServiceError::InvalidPairingKey
        | AuthServiceError::InvalidRouteTicket
        | AuthServiceError::InvalidRouteGrant => {
            report.escalate(wire::OperationError::InvalidRequest(details))
        }
        AuthServiceError::InvalidSession => {
            report.escalate(wire::OperationError::Forbidden(details))
        }
        AuthServiceError::Capacity => report.escalate(wire::OperationError::Overloaded(details)),
        AuthServiceError::Unavailable => {
            report.escalate(wire::OperationError::Unavailable(details))
        }
    }
}
