//! Control-plane implementation for host-owned browser authentication.

use async_trait::async_trait;
use axum::http::Uri;
use reportify::Report;
use tascarrel_api::types::auth as api;
use tascarrel_api::types::protocol as wire;

use crate::control_plane::ConnectionPrincipal;
use crate::control_plane::InvocationCtx;
use crate::control_plane::SubscriptionCtx;
use crate::control_plane::auth_operation_error;
use crate::control_plane::invalid_request;
use crate::control_plane::operations::EventSource;
use crate::control_plane::operations::ExecuteAction;
use crate::control_plane::operations::OpenSubscription;
use crate::services::auth::BrowserSessionSubscription;

const ROUTE_AUTHORIZATION_PATH: &str = "/.tascarrel/authorize";

#[async_trait]
impl ExecuteAction for api::CreatePairingKeyAction {
    async fn check_permissions(
        &self,
        context: &InvocationCtx<'_>,
    ) -> Result<(), Report<wire::OperationError>> {
        match context.principal() {
            ConnectionPrincipal::LocalAdmin => Ok(()),
            ConnectionPrincipal::Browser { .. } | ConnectionPrincipal::Internal => {
                Err(wire::OperationError::forbidden())
            }
        }
    }

    async fn execute(
        self,
        context: InvocationCtx<'_>,
    ) -> Result<Self::Output, Report<wire::OperationError>> {
        context
            .state()
            .auth()
            .create_pairing_key(self.label.map(String::from))
            .map_err(auth_operation_error)
    }
}

#[async_trait]
impl ExecuteAction for api::RevokeBrowserSessionAction {
    async fn check_permissions(
        &self,
        context: &InvocationCtx<'_>,
    ) -> Result<(), Report<wire::OperationError>> {
        require_browser_or_local(context.principal())
    }

    async fn execute(
        self,
        context: InvocationCtx<'_>,
    ) -> Result<Self::Output, Report<wire::OperationError>> {
        context
            .state()
            .auth()
            .revoke_session(&self.session_id)
            .await
            .map_err(auth_operation_error)?;
        Ok(api::RevokeBrowserSessionOutput {})
    }
}

#[async_trait]
impl ExecuteAction for api::CreateHttpRouteTicketAction {
    async fn check_permissions(
        &self,
        context: &InvocationCtx<'_>,
    ) -> Result<(), Report<wire::OperationError>> {
        match context.principal() {
            ConnectionPrincipal::Browser { .. } => Ok(()),
            ConnectionPrincipal::LocalAdmin | ConnectionPrincipal::Internal => {
                Err(wire::OperationError::forbidden())
            }
        }
    }

    async fn execute(
        self,
        context: InvocationCtx<'_>,
    ) -> Result<Self::Output, Report<wire::OperationError>> {
        let ConnectionPrincipal::Browser { session_id, origin } = context.principal() else {
            return Err(wire::OperationError::forbidden());
        };
        let route = context
            .state()
            .network()
            .http_route_by_hostname_prefix(&self.hostname_prefix)
            .ok_or_else(|| invalid_request("HTTP route does not exist"))?;
        let return_to = self.return_to.as_deref().unwrap_or("/").to_owned();
        let suffix = route_hostname_suffix(origin, context.state().network().hostname_suffix())?;
        let ticket = context
            .state()
            .auth()
            .create_route_ticket(
                session_id.clone(),
                route.id,
                route_hostname(self.hostname_prefix.as_str(), &suffix),
                return_to,
            )
            .map_err(auth_operation_error)?;
        let url = route_authorization_url(origin, self.hostname_prefix.as_str(), &suffix, &ticket)?;
        Ok(api::CreateHttpRouteTicketOutput { url: url.into() })
    }
}

#[async_trait]
impl OpenSubscription for api::BrowserSessionsChangedSubscription {
    async fn check_permissions(
        &self,
        context: &SubscriptionCtx<'_>,
    ) -> Result<(), Report<wire::OperationError>> {
        require_browser_or_local(context.principal())
    }

    type Source = BrowserSessionSubscription;

    async fn open(
        self,
        context: SubscriptionCtx<'_>,
    ) -> Result<Self::Source, Report<wire::OperationError>> {
        let current_session_id = match context.principal() {
            ConnectionPrincipal::Browser { session_id, .. } => Some(session_id.clone()),
            ConnectionPrincipal::LocalAdmin | ConnectionPrincipal::Internal => None,
        };
        Ok(context
            .state()
            .auth()
            .subscribe_sessions(current_session_id))
    }
}

#[async_trait]
impl EventSource for BrowserSessionSubscription {
    type Event = api::BrowserSessionsChangedEvent;

    async fn recv(&mut self) -> Result<Option<Self::Event>, Report<wire::OperationError>> {
        BrowserSessionSubscription::recv(self)
            .await
            .map(Some)
            .map_err(auth_operation_error)
    }
}

fn require_browser_or_local(
    principal: &ConnectionPrincipal,
) -> Result<(), Report<wire::OperationError>> {
    match principal {
        ConnectionPrincipal::LocalAdmin | ConnectionPrincipal::Browser { .. } => Ok(()),
        ConnectionPrincipal::Internal => Err(wire::OperationError::forbidden()),
    }
}

fn route_authorization_url(
    origin: &str,
    prefix: &str,
    suffix: &str,
    ticket: &str,
) -> Result<String, Report<wire::OperationError>> {
    let origin = origin
        .parse::<Uri>()
        .map_err(|_| invalid_request("browser session has an invalid origin"))?;
    let scheme = origin
        .scheme_str()
        .filter(|scheme| matches!(*scheme, "http" | "https"))
        .ok_or_else(|| invalid_request("browser session has an invalid origin scheme"))?;
    let authority = origin
        .authority()
        .ok_or_else(|| invalid_request("browser session has no origin authority"))?;
    let port = authority
        .port_u16()
        .map(|port| format!(":{port}"))
        .unwrap_or_default();
    Ok(format!(
        "{scheme}://{}{port}{ROUTE_AUTHORIZATION_PATH}#ticket={ticket}",
        route_hostname(prefix, suffix),
    ))
}

fn route_hostname(prefix: &str, suffix: &str) -> String {
    format!("{prefix}.{suffix}")
}

fn route_hostname_suffix(
    origin: &str,
    configured_suffix: &str,
) -> Result<String, Report<wire::OperationError>> {
    let origin = origin
        .parse::<Uri>()
        .map_err(|_| invalid_request("browser session has an invalid origin"))?;
    let local = origin.scheme_str() == Some("http")
        && origin.authority().is_some_and(|authority| {
            let host = authority.host();
            host.eq_ignore_ascii_case("localhost")
                || host.eq_ignore_ascii_case(crate::NetworkService::LOCAL_HOSTNAME_SUFFIX)
                || host
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|address| address.is_loopback())
        });
    Ok(if local {
        crate::NetworkService::LOCAL_HOSTNAME_SUFFIX
    } else {
        configured_suffix
    }
    .to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verifies remote route configuration preserves loopback development
    /// addresses while issuing public addresses to remote browsers.
    #[test]
    fn local_browser_routes_keep_the_local_development_suffix() {
        assert_eq!(
            route_hostname_suffix("http://tascarrel.localhost:8272", "tascarrel.example.com",)
                .unwrap(),
            "tascarrel.localhost"
        );
        assert_eq!(
            route_hostname_suffix("http://127.0.0.1:8272", "tascarrel.example.com").unwrap(),
            "tascarrel.localhost"
        );
        assert_eq!(
            route_hostname_suffix("https://tascarrel.example.com", "tascarrel.example.com",)
                .unwrap(),
            "tascarrel.example.com"
        );
    }
}
