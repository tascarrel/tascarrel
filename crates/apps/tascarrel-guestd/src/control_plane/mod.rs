//! Typed guest control-plane dispatch and feature composition.
//!
//! [`GuestState`] composes the services owned by one guest daemon.
//! [`GuestControlService`] terminates the persistent hostd control-plane
//! connection and dispatches typed actions and subscriptions into that state.

mod chat_subscription;
mod host;
mod impls;
mod operations;
mod service;

use std::sync::Arc;

use futures_util::future::BoxFuture;
pub(crate) use host::HostClient;
pub(crate) use host::HostClientError;
use reportify::Report;
pub use service::GuestControlService;
pub use service::GuestControlServiceConfig;
pub use service::GuestControlServiceError;
use tascarrel_api::types::protocol as wire;
use tascarrel_api::types::repositories as repository_api;

use crate::ChangesService;
use crate::ChatService;
use crate::CodeService;
use crate::FilesService;
use crate::GuestNetworkService;
use crate::GuestRepositoryManager;
use crate::GuestService;
use crate::ImageInputRefresh;
use crate::ImageService;
use crate::ProcessSupervisor;
use crate::RepositoryConfigProvider;
use crate::repositories::RepositoryPreparation;
use crate::services::pods::PodService;

/// Services and state shared by every control-plane connection to one guest.
#[derive(Clone)]
pub struct GuestState {
    services: GuestServices,
    image_input: Arc<dyn ImageInputRefresh>,
    repositories: Option<Arc<GuestRepositoryManager>>,
    repository_config: Option<Arc<dyn RepositoryConfigProvider>>,
}

impl GuestState {
    /// Creates guest state from its feature services.
    #[must_use]
    pub fn new(services: GuestServices, image_input: Arc<dyn ImageInputRefresh>) -> Self {
        Self {
            services,
            image_input,
            repositories: None,
            repository_config: None,
        }
    }

    /// Adds the operation-time repository manager and configuration provider.
    #[must_use]
    pub fn with_repositories(
        mut self,
        repositories: Option<Arc<GuestRepositoryManager>>,
        repository_config: Option<Arc<dyn RepositoryConfigProvider>>,
    ) -> Self {
        self.repositories = repositories;
        self.repository_config = repository_config;
        self
    }

    /// Returns the workspace chat feature.
    pub(crate) fn chats(&self) -> &ChatService {
        &self.services.chats
    }

    /// Returns the workspace repository changes service.
    pub(crate) fn changes(&self) -> &ChangesService {
        &self.services.changes
    }

    /// Returns the workspace Code editor lifecycle service.
    pub(crate) fn code(&self) -> &CodeService {
        &self.services.code
    }

    /// Returns the pod workspace file inspection service.
    pub(crate) fn files(&self) -> &FilesService {
        &self.services.files
    }

    /// Returns the workspace image lifecycle service.
    pub(crate) fn images(&self) -> &ImageService {
        &self.services.images
    }

    /// Returns the operation-time guest network service.
    pub(crate) fn network(&self) -> &Arc<GuestNetworkService> {
        &self.services.network
    }

    /// Returns the operation-time host image-input refresher.
    pub(crate) fn image_input(&self) -> &Arc<dyn ImageInputRefresh> {
        &self.image_input
    }

    /// Returns the guest information and resource metric service.
    pub(crate) fn guest(&self) -> &GuestService {
        &self.services.guest
    }

    /// Returns the workspace pod lifecycle service.
    pub(crate) fn pods(&self) -> &PodService {
        &self.services.pods
    }

    /// Returns the workspace-wide process supervisor.
    pub(crate) fn processes(&self) -> &ProcessSupervisor {
        &self.services.processes
    }

    /// Returns the optional repository workspace manager.
    pub(crate) fn repositories(&self) -> Option<&Arc<GuestRepositoryManager>> {
        self.repositories.as_ref()
    }

    /// Returns the operation-time repository configuration provider.
    pub(crate) fn repository_config(&self) -> Option<&Arc<dyn RepositoryConfigProvider>> {
        self.repository_config.as_ref()
    }
}

/// Feature services composed into one guest control-plane state.
#[derive(Clone)]
pub struct GuestServices {
    /// Workspace chat lifecycle and event service.
    pub chats: ChatService,
    /// Live repository status and detailed changes service.
    pub changes: ChangesService,
    /// Workspace Code editor lifecycle service.
    pub code: CodeService,
    /// Pod workspace file inspection service.
    pub files: FilesService,
    /// Guest information and metrics service.
    pub guest: GuestService,
    /// Workspace image lifecycle service.
    pub images: ImageService,
    /// Operation-time guest network service.
    pub network: Arc<GuestNetworkService>,
    /// Pod lifecycle service.
    pub pods: PodService,
    /// Workspace process supervisor.
    pub processes: ProcessSupervisor,
}

/// State and complete wire request available to one typed action.
pub(crate) struct InvocationCtx<'a> {
    state: &'a GuestState,
    host: &'a HostClient,
    invocation: &'a wire::RpcInvocation,
}

impl<'a> InvocationCtx<'a> {
    /// Creates a context for one decoded invocation.
    pub(crate) const fn new(
        state: &'a GuestState,
        host: &'a HostClient,
        invocation: &'a wire::RpcInvocation,
    ) -> Self {
        Self {
            state,
            host,
            invocation,
        }
    }

    /// Returns the guest services available to the action.
    pub(crate) const fn state(&self) -> &'a GuestState {
        self.state
    }

    /// Returns the typed client for actions owned by hostd.
    pub(crate) const fn host(&self) -> &'a HostClient {
        self.host
    }

    /// Returns the workspace addressed by this guest invocation.
    pub(crate) fn target_workspace(
        &self,
    ) -> Result<&'a tascarrel_api::types::workspaces::WorkspaceName, Report<wire::OperationError>>
    {
        match &self.invocation.target {
            wire::Address::Workspace(address) => Ok(&address.workspace),
            wire::Address::Pod(address) => Ok(&address.workspace),
            wire::Address::Host => Err(invalid_request(
                "guest operation must target a workspace or pod",
            )),
        }
    }

    /// Returns the pod addressed by this invocation.
    pub(crate) fn target_pod(&self) -> Result<&'a wire::PodAddress, Report<wire::OperationError>> {
        match &self.invocation.target {
            wire::Address::Pod(address) => Ok(address),
            wire::Address::Workspace(_) | wire::Address::Host => {
                Err(invalid_request("pod operation must target a pod"))
            }
        }
    }

    /// Creates a workspace-authenticated context for one causally nested host
    /// action.
    pub(crate) fn nested_host_request_context(
        &self,
    ) -> Result<wire::RequestContext, Report<wire::OperationError>> {
        let parent = self.require_routing_context()?;
        let workspace = match &self.invocation.target {
            wire::Address::Workspace(address) => address.clone(),
            wire::Address::Pod(address) => wire::WorkspaceAddress {
                workspace: address.workspace.clone(),
            },
            wire::Address::Host => {
                return Err(invalid_request(
                    "guest operation cannot create a host request from a host target",
                ));
            }
        };
        let actor = wire::Actor::Workspace(workspace);
        Ok(wire::RequestContext {
            origin: actor.clone(),
            caller: actor,
            trace_id: parent.trace_id.clone(),
            caused_by: Some(self.invocation.id.0.clone()),
        })
    }

    /// Requires the routing context attached to the invocation.
    pub(crate) fn require_routing_context(
        &self,
    ) -> Result<&'a wire::RequestContext, Report<wire::OperationError>> {
        self.invocation
            .context
            .as_ref()
            .ok_or_else(|| invalid_request("validated guest operation has no routing context"))
    }

    /// Prepares host repository caches and captures operation-scoped guest
    /// reconciliation dependencies without refreshing ready upstreams.
    pub(crate) async fn repository_preparation(
        &self,
    ) -> Result<Option<RepositoryPreparation>, Report<wire::OperationError>> {
        self.repository_preparation_task()?.await
    }

    /// Captures everything required to prepare repositories after a creation
    /// action returns its resource identifiers.
    pub(crate) fn repository_preparation_task(
        &self,
    ) -> Result<RepositoryPreparationTask, Report<wire::OperationError>> {
        let Some(manager) = self.state.repositories().cloned() else {
            return Ok(Box::pin(std::future::ready(Ok(None))));
        };
        let repository_config = self.state.repository_config().cloned();
        let request_context = self.nested_host_request_context()?;
        let workspace = self.target_workspace()?.clone();
        let host = self.host.clone();
        Ok(Box::pin(async move {
            let repository_config = manager
                .capture_repositories(repository_config.as_deref())
                .await
                .map_err(|error| {
                    internal_error(format!(
                        "failed to refresh repository configuration: {error}"
                    ))
                })?;
            let output = host
                .execute(
                    request_context,
                    repository_api::PrepareRepositorySnapshotAction { workspace },
                )
                .await
                .map_err(|error| {
                    internal_error(format!("failed to prepare repository caches: {error}"))
                })?;
            RepositoryPreparation::new_versioned(
                manager,
                repository_config,
                output.repositories.iter().cloned(),
            )
            .map(Some)
            .map_err(|error| {
                internal_error(format!(
                    "failed to validate repository cache snapshot: {error}"
                ))
            })
        }))
    }
}

/// State and complete wire request available to one typed subscription.
pub(crate) struct SubscriptionCtx<'a> {
    state: &'a GuestState,
    subscription: &'a wire::SubscriptionStart,
}

impl<'a> SubscriptionCtx<'a> {
    /// Creates a context for one decoded subscription request.
    pub(crate) const fn new(
        state: &'a GuestState,
        subscription: &'a wire::SubscriptionStart,
    ) -> Self {
        Self {
            state,
            subscription,
        }
    }

    /// Returns the guest services available to the subscription.
    pub(crate) const fn state(&self) -> &'a GuestState {
        self.state
    }

    /// Returns the workspace addressed by this guest subscription.
    pub(crate) fn target_workspace(
        &self,
    ) -> Result<&'a tascarrel_api::types::workspaces::WorkspaceName, Report<wire::OperationError>>
    {
        match &self.subscription.target {
            wire::Address::Workspace(address) => Ok(&address.workspace),
            wire::Address::Pod(address) => Ok(&address.workspace),
            wire::Address::Host => Err(invalid_request(
                "guest subscription must target a workspace or pod",
            )),
        }
    }

    /// Returns the pod addressed by this subscription.
    pub(crate) fn target_pod(&self) -> Result<&'a wire::PodAddress, Report<wire::OperationError>> {
        match &self.subscription.target {
            wire::Address::Pod(address) => Ok(address),
            wire::Address::Workspace(_) | wire::Address::Host => {
                Err(invalid_request("pod subscription must target a pod"))
            }
        }
    }

    /// Requires the routing context attached to the subscription.
    pub(crate) fn require_routing_context(
        &self,
    ) -> Result<&'a wire::RequestContext, Report<wire::OperationError>> {
        self.subscription
            .context
            .as_ref()
            .ok_or_else(|| invalid_request("operation has no routing context"))
    }
}

/// Repository preparation captured from an action for deferred execution.
pub(crate) type RepositoryPreparationTask =
    BoxFuture<'static, Result<Option<RepositoryPreparation>, Report<wire::OperationError>>>;

/// Creates a contract operation error.
fn invalid_request(message: impl Into<String>) -> Report<wire::OperationError> {
    Report::new(wire::OperationError::InvalidRequest(
        operation_error_details(message),
    ))
}

/// Creates an internal operation error.
fn internal_error(message: impl Into<String>) -> Report<wire::OperationError> {
    Report::new(wire::OperationError::Internal(operation_error_details(
        message,
    )))
}

/// Creates operation details for a standalone diagnostic message.
fn operation_error_details(message: impl Into<String>) -> wire::OperationErrorDetails {
    let message = message.into();
    wire::OperationErrorDetails {
        message: message.clone().into(),
        report: None,
    }
}
