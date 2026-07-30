//! Repository-input transfer for durable host operations.

use serde::Deserialize;
use serde::Serialize;

use super::PodId;
use super::RemoteError;

/// Pod-to-guest endpoint for transferring one host-operation input.
pub const MUX_POD_HOST_OPERATION_INPUT_ENDPOINT: &str = "tascarrel-pod-host-operation-input-v1";
/// Guest-to-host endpoint for transferring one authenticated host-operation
/// input.
pub const MUX_HOST_OPERATION_INPUT_ENDPOINT: &str = "tascarrel-host-operation-input-v1";

/// Maximum accepted Git bundle size for one input.
pub const MAX_HOST_OPERATION_INPUT_BYTES: u64 = 64 * 1024 * 1024 * 1024;

/// Prefix used by the trusted pod helper to correlate an Automation
/// host-command process with its durable host operation.
pub const AUTOMATION_HOST_OPERATION_MARKER_PREFIX: &str = "::tascarrel-automation-host-operation::";

/// Untrusted input header supplied by the requesting pod.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PodHostOperationInputRequest {
    /// Durable operation receiving this input.
    pub operation_id: String,
    /// Declared input name.
    pub input_name: String,
    /// Exact Git commit carried by the bundle.
    pub revision: String,
    /// Original `HEAD` used to produce a change summary, when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_revision: Option<String>,
    /// Exact number of raw bundle bytes following the ready response.
    pub length: u64,
}

/// Guest-authenticated input header sent to hostd.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HostOperationInputRequest {
    /// Pod identity established by guestd's private listener.
    pub pod_id: PodId,
    /// Untrusted request details.
    pub input: PodHostOperationInputRequest,
}

/// Framed responses before and after the raw bundle body.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum HostOperationInputResponse {
    /// The receiver validated the header and is ready for the declared bytes.
    Ready,
    /// The bundle was durably retained, verified, and materialized.
    Completed,
    /// The request or transferred bundle was rejected.
    Error { error: RemoteError },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verifies that untrusted transfer headers reject undeclared fields.
    #[test]
    fn input_request_rejects_unknown_fields() {
        let request = r#"{
            "operation_id":"host_operation_123",
            "input_name":"source",
            "revision":"0123456789abcdef",
            "length":42,
            "unexpected":true
        }"#;
        assert!(serde_json::from_str::<PodHostOperationInputRequest>(request).is_err());
    }
}
