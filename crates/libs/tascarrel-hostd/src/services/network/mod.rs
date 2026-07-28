//! Host-owned semantic DNS, TCP egress, HTTP routing, and port forwarding.
//!
//! [`NetworkService`] owns the host resolver, attributed guest transports,
//! per-workspace activity streams, route identities, loopback listeners, and
//! HTTP forwarding into workspace pods.

mod activity;
pub(crate) mod policy;
mod proxy;
mod service;
mod transport;

pub(crate) use policy::NetworkPolicy;
pub use service::DnsRequestsSubscription;
pub use service::HttpRouteListSubscription;
pub use service::NetworkProxyError;
pub use service::NetworkService;
pub use service::NetworkServiceConfig;
pub use service::NetworkServiceError;
pub use service::PodHostForwardListSubscription;
pub use service::PortForwardListSubscription;
pub(crate) use service::ResolvedHttpRoute;
pub use service::TcpFlowsSubscription;
pub(crate) use service::validate_hostname_suffix;
