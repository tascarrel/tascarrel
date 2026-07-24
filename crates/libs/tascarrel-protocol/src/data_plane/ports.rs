//! Requests for publishing pod ports through the host.

use serde::Deserialize;
use serde::Serialize;

use super::PodId;
use super::RemoteError;

/// Guest-to-host lease for one host-loopback published pod port.
pub const MUX_PUBLISH_HOST_ENDPOINT: &str = "tascarrel-publish-host-v1";
/// Host-to-guest connection for one leased published pod port.
pub const MUX_PUBLISH_GUEST_ENDPOINT: &str = "tascarrel-publish-guest-v1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublishRequest {
    pub pod_id: PodId,
    pub pod_port: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default)]
    pub tab: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublishResponse {
    pub result: Result<u16, RemoteError>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublishedPortConnect {
    pub pod_id: PodId,
    pub pod_port: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublishedPortConnectResponse {
    pub result: Result<(), RemoteError>,
}
