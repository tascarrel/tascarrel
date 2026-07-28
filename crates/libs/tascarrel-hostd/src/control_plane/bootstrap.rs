//! Pairing-key control service available while hostd is starting.

use reportify::ErrorExt as _;
use reportify::Report;
use tascarrel_api::types::auth;
use tascarrel_api::types::protocol as wire;
use tascarrel_protocol::control_plane;
use tascarrel_protocol::control_plane::Transport;
use tascarrel_protocol::control_plane::policy::topology;
use tascarrel_protocol::control_plane::server;

use super::auth_operation_error;
use super::service::decode_typed;
use crate::services::auth::AuthService;

/// Restricted local control service available before host services are ready.
#[derive(Clone)]
pub(crate) struct BootstrapControlService {
    auth: AuthService,
}

impl BootstrapControlService {
    /// Creates a startup service backed by the initialized authentication
    /// state.
    pub(crate) const fn new(auth: AuthService) -> Self {
        Self { auth }
    }

    /// Serves pairing requests until the local control link closes.
    pub(crate) async fn serve<T>(
        self,
        transport: T,
        client_id: wire::ClientId,
    ) -> control_plane::Result<()>
    where
        T: Transport + 'static,
    {
        server::Server::new(self, BootstrapRouter)
            .serve(
                transport,
                topology::client_to_hostd(&client_id),
                control_plane::Config::default(),
            )
            .await
    }
}

impl server::Service for BootstrapControlService {
    fn invoke(
        &self,
        invocation: wire::RpcInvocation,
    ) -> server::OperationFuture<'static, serde_json::Value> {
        let auth = self.auth.clone();
        Box::pin(async move {
            if invocation.procedure.as_ref() != "auth_CreatePairingKey" {
                return Err(unavailable("the Tascarrel host is still starting"));
            }
            let input = decode_typed::<auth::CreatePairingKeyAction>(invocation.input)?;
            let output = auth
                .create_pairing_key(input.label.map(String::from))
                .map_err(auth_operation_error)?;
            serde_json::to_value(output).map_err(|error| unavailable(error.to_string()))
        })
    }

    fn subscribe(
        &self,
        _subscription: wire::SubscriptionStart,
    ) -> server::OperationFuture<'static, Box<dyn server::EventSource>> {
        Box::pin(async { Err(unavailable("the Tascarrel host is still starting")) })
    }
}

#[derive(Clone, Copy)]
struct BootstrapRouter;

impl server::Router for BootstrapRouter {
    fn resolve(&self, target: wire::Address) -> server::OperationFuture<'static, server::Route> {
        Box::pin(async move {
            if target == wire::Address::Host {
                Ok(server::Route::Local)
            } else {
                Err(unavailable("the Tascarrel host is still starting"))
            }
        })
    }
}

fn unavailable(message: impl Into<String>) -> Report<wire::OperationError> {
    wire::OperationError::Unavailable(error_details(message)).report()
}

fn error_details(message: impl Into<String>) -> wire::OperationErrorDetails {
    wire::OperationErrorDetails {
        message: message.into().into(),
        report: None,
    }
}
