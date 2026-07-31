//! Pod submission and observation of durable overlay-share approval requests.

use tascarrel_api::types::shares as api;

use crate::client::PodClient;
use crate::error::PodctlResult;

/// Submits one exact current overlay revision for host approval.
#[tracing::instrument(level = "debug", skip(client), fields(share = %share), err)]
pub(crate) async fn submit(
    client: &PodClient,
    share: String,
) -> PodctlResult<api::ShareOverlayApprovalRequest> {
    client
        .invoke_host(api::RequestShareOverlayApprovalAction {
            workspace: client.identity().workspace.clone(),
            pod_id: client.identity().pod_id.clone(),
            share: share.into(),
        })
        .await
        .map(|output| output.request)
}

/// Returns pending overlay approvals submitted by the current pod.
#[tracing::instrument(level = "debug", skip(client), err)]
pub(crate) async fn list(client: &PodClient) -> PodctlResult<api::ShareOverlayApprovalRequestList> {
    client
        .first_host_event(api::ShareOverlayApprovalRequestListChangedSubscription {
            workspace: client.identity().workspace.clone(),
            pod_id: Some(client.identity().pod_id.clone()),
            cursor: None,
        })
        .await
        .map(|event| event.value)
}

/// Withdraws one pending approval owned by the current pod.
#[tracing::instrument(level = "debug", skip(client), fields(approval_id = %approval_id.0), err)]
pub(crate) async fn cancel(
    client: &PodClient,
    approval_id: api::ShareOverlayApprovalId,
) -> PodctlResult<()> {
    client
        .invoke_host(api::CancelShareOverlayApprovalAction {
            workspace: client.identity().workspace.clone(),
            pod_id: client.identity().pod_id.clone(),
            approval_id,
        })
        .await?;
    Ok(())
}
