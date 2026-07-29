//! Control-plane implementation for host-owned network routes and forwards.

use async_trait::async_trait;
use reportify::Report;
use tascarrel_api::types::network as api;
use tascarrel_api::types::protocol as wire;

use crate::control_plane::InvocationCtx;
use crate::control_plane::SubscriptionCtx;
use crate::control_plane::operation_error_details;
use crate::control_plane::operations::EventSource;
use crate::control_plane::operations::ExecuteAction;
use crate::control_plane::operations::OpenSubscription;
use crate::control_plane::operations::store_event;
use crate::services::network::DnsRequestsSubscription;
use crate::services::network::HttpRequestsSubscription;
use crate::services::network::HttpRouteListSubscription;
use crate::services::network::NetworkServiceError;
use crate::services::network::PodHostForwardListSubscription;
use crate::services::network::PortForwardListSubscription;
use crate::services::network::TcpFlowsSubscription;

#[async_trait]
impl ExecuteAction for api::GetPodHttpRoutesAction {
    async fn check_permissions(
        &self,
        context: &InvocationCtx<'_>,
    ) -> Result<(), Report<wire::OperationError>> {
        require_route_target(
            &context.require_routing_context()?.caller,
            &self.workspace,
            &self.pod_id,
        )
    }

    async fn execute(
        self,
        context: InvocationCtx<'_>,
    ) -> Result<Self::Output, Report<wire::OperationError>> {
        context
            .state()
            .network()
            .get_pod_http_routes(&self, context.state().workspaces())
            .map_err(network_error)
    }
}

#[async_trait]
impl ExecuteAction for api::GetPodPortForwardsAction {
    async fn check_permissions(
        &self,
        context: &InvocationCtx<'_>,
    ) -> Result<(), Report<wire::OperationError>> {
        require_route_target(
            &context.require_routing_context()?.caller,
            &self.workspace,
            &self.pod_id,
        )
    }

    async fn execute(
        self,
        context: InvocationCtx<'_>,
    ) -> Result<Self::Output, Report<wire::OperationError>> {
        context
            .state()
            .network()
            .get_pod_port_forwards(&self, context.state().workspaces())
            .map_err(network_error)
    }
}

#[async_trait]
impl ExecuteAction for api::CreatePortForwardAction {
    async fn check_permissions(
        &self,
        context: &InvocationCtx<'_>,
    ) -> Result<(), Report<wire::OperationError>> {
        require_route_target(
            &context.require_routing_context()?.caller,
            &self.workspace,
            &self.pod_id,
        )
    }

    async fn execute(
        self,
        context: InvocationCtx<'_>,
    ) -> Result<Self::Output, Report<wire::OperationError>> {
        context
            .state()
            .network()
            .create_port_forward(self, context.state().workspaces())
            .await
            .map_err(network_error)
    }
}

#[async_trait]
impl ExecuteAction for api::DeletePortForwardAction {
    async fn check_permissions(
        &self,
        context: &InvocationCtx<'_>,
    ) -> Result<(), Report<wire::OperationError>> {
        let caller = &context.require_routing_context()?.caller;
        if caller.is_host_or_client() {
            return Ok(());
        }
        let forward = context
            .state()
            .network()
            .port_forward(&self.port_forward_id)
            .ok_or_else(wire::OperationError::forbidden)?;
        require_route_target(caller, &forward.workspace, &forward.pod_id)
    }

    async fn execute(
        self,
        context: InvocationCtx<'_>,
    ) -> Result<Self::Output, Report<wire::OperationError>> {
        context
            .state()
            .network()
            .delete_port_forward(&self)
            .await
            .map_err(network_error)
    }
}

#[async_trait]
impl ExecuteAction for api::CreatePodHostForwardAction {
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
            .network()
            .create_pod_host_forward(self, context.state().workspaces())
            .map_err(network_error)
    }
}

#[async_trait]
impl ExecuteAction for api::DeletePodHostForwardAction {
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
            .network()
            .delete_pod_host_forward(&self)
            .map_err(network_error)
    }
}

#[async_trait]
impl ExecuteAction for api::CreateHttpRouteAction {
    async fn check_permissions(
        &self,
        context: &InvocationCtx<'_>,
    ) -> Result<(), Report<wire::OperationError>> {
        require_route_target(
            &context.require_routing_context()?.caller,
            &self.workspace,
            &self.pod_id,
        )
    }

    async fn execute(
        self,
        context: InvocationCtx<'_>,
    ) -> Result<Self::Output, Report<wire::OperationError>> {
        context
            .state()
            .network()
            .create_http_route(self, context.state().workspaces())
            .map_err(network_error)
    }
}

#[async_trait]
impl ExecuteAction for api::SetHttpRouteTrustedTascarrelFrontendAction {
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
            .network()
            .set_http_route_trusted_tascarrel_frontend(&self)
            .map_err(network_error)
    }
}

#[async_trait]
impl ExecuteAction for api::DeleteHttpRouteAction {
    async fn check_permissions(
        &self,
        context: &InvocationCtx<'_>,
    ) -> Result<(), Report<wire::OperationError>> {
        let caller = &context.require_routing_context()?.caller;
        if caller.is_host_or_client() {
            return Ok(());
        }
        let route = context
            .state()
            .network()
            .http_route(&self.http_route_id)
            .ok_or_else(wire::OperationError::forbidden)?;
        require_route_target(caller, &route.workspace, &route.pod_id)
    }

    async fn execute(
        self,
        context: InvocationCtx<'_>,
    ) -> Result<Self::Output, Report<wire::OperationError>> {
        context
            .state()
            .network()
            .delete_http_route(&self)
            .map_err(network_error)
    }
}

#[async_trait]
impl OpenSubscription for api::HttpRouteListChangedSubscription {
    async fn check_permissions(
        &self,
        context: &SubscriptionCtx<'_>,
    ) -> Result<(), Report<wire::OperationError>> {
        require_workspace_reader(&context.require_routing_context()?.caller, &self.workspace)
    }

    type Source = HttpRouteListSubscription;

    async fn open(
        self,
        context: SubscriptionCtx<'_>,
    ) -> Result<Self::Source, Report<wire::OperationError>> {
        context
            .state()
            .network()
            .subscribe_http_routes(&self, context.state().workspaces())
            .map_err(network_error)
    }
}

#[async_trait]
impl EventSource for HttpRouteListSubscription {
    type Event = api::HttpRouteListChangedEvent;

    async fn recv(&mut self) -> Result<Option<Self::Event>, Report<wire::OperationError>> {
        Ok(tascarrel_store::Subscription::recv(self)
            .await
            .map(|change| api::HttpRouteListChangedEvent {
                change: store_event(change),
            }))
    }
}

#[async_trait]
impl OpenSubscription for api::PortForwardListChangedSubscription {
    async fn check_permissions(
        &self,
        context: &SubscriptionCtx<'_>,
    ) -> Result<(), Report<wire::OperationError>> {
        require_host_or_client(&context.require_routing_context()?.caller)
    }

    type Source = PortForwardListSubscription;

    async fn open(
        self,
        context: SubscriptionCtx<'_>,
    ) -> Result<Self::Source, Report<wire::OperationError>> {
        context
            .state()
            .network()
            .subscribe_port_forwards(&self, context.state().workspaces())
            .map_err(network_error)
    }
}

#[async_trait]
impl EventSource for PortForwardListSubscription {
    type Event = api::PortForwardListChangedEvent;

    async fn recv(&mut self) -> Result<Option<Self::Event>, Report<wire::OperationError>> {
        Ok(tascarrel_store::Subscription::recv(self)
            .await
            .map(|change| api::PortForwardListChangedEvent {
                change: store_event(change),
            }))
    }
}

#[async_trait]
impl OpenSubscription for api::PodHostForwardListChangedSubscription {
    async fn check_permissions(
        &self,
        context: &SubscriptionCtx<'_>,
    ) -> Result<(), Report<wire::OperationError>> {
        require_host_or_client(&context.require_routing_context()?.caller)
    }

    type Source = PodHostForwardListSubscription;

    async fn open(
        self,
        context: SubscriptionCtx<'_>,
    ) -> Result<Self::Source, Report<wire::OperationError>> {
        context
            .state()
            .network()
            .subscribe_pod_host_forwards(&self, context.state().workspaces())
            .map_err(network_error)
    }
}

#[async_trait]
impl EventSource for PodHostForwardListSubscription {
    type Event = api::PodHostForwardListChangedEvent;

    async fn recv(&mut self) -> Result<Option<Self::Event>, Report<wire::OperationError>> {
        Ok(tascarrel_store::Subscription::recv(self)
            .await
            .map(|change| api::PodHostForwardListChangedEvent {
                change: store_event(change),
            }))
    }
}

#[async_trait]
impl OpenSubscription for api::DnsRequestsSubscription {
    async fn check_permissions(
        &self,
        context: &SubscriptionCtx<'_>,
    ) -> Result<(), Report<wire::OperationError>> {
        require_host_or_client(&context.require_routing_context()?.caller)
    }

    type Source = DnsRequestsSubscription;

    async fn open(
        self,
        context: SubscriptionCtx<'_>,
    ) -> Result<Self::Source, Report<wire::OperationError>> {
        context
            .state()
            .network()
            .subscribe_dns_requests(&self, context.state().workspaces())
            .map_err(network_error)
    }
}

#[async_trait]
impl EventSource for DnsRequestsSubscription {
    type Event = api::DnsRequestsEvent;

    async fn recv(&mut self) -> Result<Option<Self::Event>, Report<wire::OperationError>> {
        Ok(self.recv().await.map(|batch| api::DnsRequestsEvent {
            cursor: api::DnsRequestCursor {
                host_instance_id: self.host_instance_id().clone(),
                position: batch.position,
            },
            requests: batch.entries.into(),
        }))
    }
}

#[async_trait]
impl OpenSubscription for api::HttpRequestsSubscription {
    async fn check_permissions(
        &self,
        context: &SubscriptionCtx<'_>,
    ) -> Result<(), Report<wire::OperationError>> {
        require_host_or_client(&context.require_routing_context()?.caller)
    }

    type Source = HttpRequestsSubscription;

    async fn open(
        self,
        context: SubscriptionCtx<'_>,
    ) -> Result<Self::Source, Report<wire::OperationError>> {
        context
            .state()
            .network()
            .subscribe_http_requests(&self, context.state().workspaces())
            .map_err(network_error)
    }
}

#[async_trait]
impl EventSource for HttpRequestsSubscription {
    type Event = api::HttpRequestsEvent;

    async fn recv(&mut self) -> Result<Option<Self::Event>, Report<wire::OperationError>> {
        Ok(self.recv().await.map(|batch| api::HttpRequestsEvent {
            cursor: api::HttpRequestCursor {
                host_instance_id: self.host_instance_id().clone(),
                position: batch.position,
            },
            requests: batch.entries.into(),
        }))
    }
}

#[async_trait]
impl OpenSubscription for api::TcpFlowsSubscription {
    async fn check_permissions(
        &self,
        context: &SubscriptionCtx<'_>,
    ) -> Result<(), Report<wire::OperationError>> {
        require_host_or_client(&context.require_routing_context()?.caller)
    }

    type Source = TcpFlowsSubscription;

    async fn open(
        self,
        context: SubscriptionCtx<'_>,
    ) -> Result<Self::Source, Report<wire::OperationError>> {
        context
            .state()
            .network()
            .subscribe_tcp_flows(&self, context.state().workspaces())
            .map_err(network_error)
    }
}

#[async_trait]
impl EventSource for TcpFlowsSubscription {
    type Event = api::TcpFlowsEvent;

    async fn recv(&mut self) -> Result<Option<Self::Event>, Report<wire::OperationError>> {
        Ok(self.recv().await.map(|batch| api::TcpFlowsEvent {
            cursor: api::TcpFlowCursor {
                host_instance_id: self.host_instance_id().clone(),
                position: batch.position,
            },
            events: batch.entries.into(),
        }))
    }
}

fn require_host_or_client(caller: &wire::Actor) -> Result<(), Report<wire::OperationError>> {
    if caller.is_host_or_client() {
        Ok(())
    } else {
        Err(wire::OperationError::forbidden())
    }
}

fn require_route_target(
    caller: &wire::Actor,
    workspace: &tascarrel_api::types::workspaces::WorkspaceName,
    pod_id: &tascarrel_api::types::pods::PodId,
) -> Result<(), Report<wire::OperationError>> {
    let allowed = caller.is_host_or_client()
        || matches!(
            caller,
            wire::Actor::Workspace(address) if &address.workspace == workspace
        )
        || matches!(
            caller,
            wire::Actor::Pod(address)
                if &address.workspace == workspace && &address.pod_id == pod_id
        );
    if allowed {
        Ok(())
    } else {
        Err(wire::OperationError::forbidden())
    }
}

fn require_workspace_reader(
    caller: &wire::Actor,
    workspace: &tascarrel_api::types::workspaces::WorkspaceName,
) -> Result<(), Report<wire::OperationError>> {
    if caller.is_host_or_client()
        || matches!(
            caller,
            wire::Actor::Workspace(address) if &address.workspace == workspace
        )
    {
        Ok(())
    } else {
        Err(wire::OperationError::forbidden())
    }
}

fn network_error(report: Report<NetworkServiceError>) -> Report<wire::OperationError> {
    let details = operation_error_details(report.to_string());
    let error = match report.error() {
        NetworkServiceError::InvalidConfiguration | NetworkServiceError::Internal(_) => {
            wire::OperationError::Internal(details)
        }
        NetworkServiceError::InvalidRequest(_) => wire::OperationError::InvalidRequest(details),
        NetworkServiceError::Unavailable(_) => wire::OperationError::Unavailable(details),
        NetworkServiceError::Overloaded(_) => wire::OperationError::Overloaded(details),
    };
    report.escalate(error)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verifies route mutations are limited to a guest actor's own target.
    #[test]
    fn guest_actors_can_only_create_routes_scoped_to_themselves() {
        let alpha = tascarrel_api::types::workspaces::WorkspaceName::new("alpha");
        let beta = tascarrel_api::types::workspaces::WorkspaceName::new("beta");
        let pod_id = tascarrel_api::types::pods::PodId::generate();
        let other_pod_id = tascarrel_api::types::pods::PodId::generate();
        let workspace_actor = wire::Actor::Workspace(wire::WorkspaceAddress {
            workspace: alpha.clone(),
        });
        let pod_actor = wire::Actor::Pod(wire::PodAddress {
            workspace: alpha.clone(),
            pod_id: pod_id.clone(),
        });

        assert!(require_route_target(&workspace_actor, &alpha, &pod_id).is_ok());
        assert!(require_route_target(&pod_actor, &alpha, &pod_id).is_ok());
        assert!(require_route_target(&workspace_actor, &beta, &pod_id).is_err());
        assert!(require_route_target(&pod_actor, &alpha, &other_pod_id).is_err());
        assert!(require_route_target(&pod_actor, &beta, &pod_id).is_err());
    }

    /// Verifies pod actors cannot observe workspace-wide route inventory.
    #[test]
    fn pod_actors_cannot_subscribe_to_workspace_route_inventory() {
        let workspace = tascarrel_api::types::workspaces::WorkspaceName::new("alpha");
        let workspace_actor = wire::Actor::Workspace(wire::WorkspaceAddress {
            workspace: workspace.clone(),
        });
        let pod_actor = wire::Actor::Pod(wire::PodAddress {
            workspace: workspace.clone(),
            pod_id: tascarrel_api::types::pods::PodId::generate(),
        });

        assert!(require_workspace_reader(&workspace_actor, &workspace).is_ok());
        assert!(require_workspace_reader(&pod_actor, &workspace).is_err());
    }
}
