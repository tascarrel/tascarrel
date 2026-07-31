//! Typed operation registries and extensions for protocol error values.

use crate::types;

impl types::protocol::Actor {
    /// Returns whether this actor is the host daemon or one of its clients.
    #[must_use]
    pub const fn is_host_or_client(&self) -> bool {
        matches!(self, Self::Client(_) | Self::Host)
    }
}

impl types::protocol::OperationError {
    /// Creates a reported permission-denied operation error.
    #[track_caller]
    #[must_use]
    pub fn forbidden() -> reportify::Report<Self> {
        reportify::Report::new(Self::Forbidden(types::protocol::OperationErrorDetails {
            message: "this operation is not allowed".into(),
            report: None,
        }))
    }

    /// Returns the human-readable details carried by this error.
    fn details(&self) -> &types::protocol::OperationErrorDetails {
        match self {
            Self::InvalidRequest(details)
            | Self::Forbidden(details)
            | Self::Unavailable(details)
            | Self::Overloaded(details)
            | Self::TimedOut(details)
            | Self::Internal(details) => details,
        }
    }
}

impl std::fmt::Display for types::protocol::OperationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let details = self.details();
        formatter.write_str(
            details
                .report
                .as_ref()
                .map_or(details.message.as_ref(), AsRef::as_ref),
        )
    }
}

impl std::error::Error for types::protocol::OperationError {}

/// Expands a callback over the actions or subscriptions implemented by guestd.
///
/// Action entries contain the procedure name, input type, and output type.
/// Subscription entries contain the subscription name, input type, and event
/// type.
#[macro_export]
macro_rules! with_guestd_operations {
    (actions => $macro:ident) => {
        $crate::__with_guestd_actions! { $macro }
    };
    (subscriptions => $macro:ident) => {
        $crate::__with_guestd_subscriptions! { $macro }
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __with_guestd_actions {
    ($macro:ident) => {
        $crate::__with_guestd_actions! { @append $macro {} }
    };
    (@append $macro:ident { $($entries:tt)* }) => {
        $macro! {
            $($entries)*
            ("guest_QueryInformation", guest::QueryGuestInformationAction, guest::QueryGuestInformationOutput),
            ("files_ReadDirectory", files::ReadDirectoryAction, files::ReadDirectoryOutput),
            ("changes_GetDivergentCommits", changes::GetDivergentCommitsAction, changes::GetDivergentCommitsOutput),
            ("changes_GetChangeSet", changes::GetChangeSetAction, changes::GetChangeSetOutput),
            ("code_EnsureSession", code::EnsureCodeSessionAction, code::EnsureCodeSessionOutput),
            ("code_DeleteSession", code::DeleteCodeSessionAction, code::DeleteCodeSessionOutput),
            ("chats_GetPodChats", chats::GetPodChatsAction, chats::GetPodChatsOutput),
            ("chats_GetUsageReport", chats::GetChatUsageReportAction, chats::GetChatUsageReportOutput),
            ("chats_Create", chats::CreateChatAction, chats::CreateChatOutput),
            ("chats_CreatePodChat", chats::CreatePodChatAction, chats::CreatePodChatOutput),
            ("chats_AttachBinding", chats::AttachChatBindingAction, chats::AttachChatBindingOutput),
            ("chats_DetachBinding", chats::DetachChatBindingAction, chats::DetachChatBindingOutput),
            ("chats_Archive", chats::ArchiveChatAction, chats::ArchiveChatOutput),
            ("chats_SetCostCenter", chats::SetChatCostCenterAction, chats::SetChatCostCenterOutput),
            ("chats_AcknowledgeAttention", chats::AcknowledgeChatAttentionAction, chats::AcknowledgeChatAttentionOutput),
            ("chats_InstallHarness", chats::InstallChatHarnessAction, chats::InstallChatHarnessOutput),
            ("chats_StartHarnessAuth", chats::StartChatHarnessAuthAction, chats::StartChatHarnessAuthOutput),
            ("chats_ValidateHarnessCredentials", chats::ValidateChatHarnessCredentialsAction, chats::ValidateChatHarnessCredentialsOutput),
            ("chats_CancelHarnessAuth", chats::CancelChatHarnessAuthAction, chats::CancelChatHarnessAuthOutput),
            ("chats_LogoutHarness", chats::LogoutChatHarnessAction, chats::LogoutChatHarnessOutput),
            ("chats_SendPrompt", chats::SendChatPromptAction, chats::SendChatPromptOutput),
            ("chats_FlushPromptQueue", chats::FlushChatPromptQueueAction, chats::FlushChatPromptQueueOutput),
            ("chats_RemoveQueuedPrompt", chats::RemoveChatQueuedPromptAction, chats::RemoveChatQueuedPromptOutput),
            ("chats_Interrupt", chats::InterruptChatAction, chats::InterruptChatOutput),
            ("chats_CompactContext", chats::CompactChatContextAction, chats::CompactChatContextOutput),
            ("chats_ResolveRequest", chats::ResolveChatRequestAction, chats::ResolveChatRequestOutput),
            ("images_Build", images::BuildImageAction, images::BuildImageOutput),
            ("images_UpdateWorkspaceSeed", images::UpdateImageWorkspaceSeedAction, images::UpdateImageWorkspaceSeedOutput),
            ("pods_Create", pods::CreatePodAction, pods::CreatePodOutput),
            ("pods_Start", pods::StartPodAction, pods::StartPodOutput),
            ("pods_Stop", pods::StopPodAction, pods::StopPodOutput),
            ("pods_Destroy", pods::DestroyPodAction, pods::DestroyPodOutput),
            ("pods_SetTitle", pods::SetPodTitleAction, pods::SetPodTitleOutput),
            ("pods_ImportRepository", pods::ImportPodRepositoryAction, pods::ImportPodRepositoryOutput),
            ("processes_GetPodProcesses", processes::GetPodProcessesAction, processes::GetPodProcessesOutput),
            ("processes_Spawn", processes::SpawnProcessAction, processes::SpawnProcessOutput),
            ("processes_SpawnTerminal", processes::SpawnProcessTerminalAction, processes::SpawnProcessTerminalOutput),
            ("processes_Kill", processes::KillProcessAction, processes::KillProcessOutput),
            ("processes_WriteTerminal", processes::WriteProcessTerminalAction, processes::WriteProcessTerminalOutput),
            ("processes_ResizeTerminal", processes::ResizeProcessTerminalAction, processes::ResizeProcessTerminalOutput),
            ("processes_Remove", processes::RemoveProcessAction, processes::RemoveProcessOutput),
            ("processes_SnapshotTerminal", processes::SnapshotProcessTerminalAction, processes::SnapshotProcessTerminalOutput),
        }
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __with_guestd_subscriptions {
    ($macro:ident) => {
        $crate::__with_guestd_subscriptions! { @append $macro {} }
    };
    (@append $macro:ident { $($entries:tt)* }) => {
        $macro! {
            $($entries)*
            ("guest_Metrics", guest::GuestMetricsSubscription, guest::GuestMetricsEvent),
            ("changes_Changed", changes::RepositoryStatusListChangedSubscription, changes::RepositoryStatusListChangedEvent),
            ("code_Changed", code::CodeSessionListChangedSubscription, code::CodeSessionListChangedEvent),
            ("chats_Changed", chats::ChatListChangedSubscription, chats::ChatListChangedEvent),
            ("chats_Chat", chats::ChatSubscription, chats::ChatEvent),
            ("chats_HarnessList", chats::ChatHarnessListSubscription, chats::ChatHarnessListEvent),
            ("chats_UsageReport", chats::ChatUsageReportSubscription, chats::ChatUsageReportEvent),
            ("images_Changed", images::ImageListChangedSubscription, images::ImageListChangedEvent),
            ("images_Log", images::ImageLogSubscription, images::ImageLogEvent),
            ("pods_Changed", pods::PodListChangedSubscription, pods::PodListChangedEvent),
            ("processes_Changed", processes::ProcessListChangedSubscription, processes::ProcessListChangedEvent),
            ("processes_Log", processes::ProcessLogSubscription, processes::ProcessLogEvent),
            ("processes_Terminal", processes::ProcessTerminalSubscription, processes::ProcessTerminalEvent),
        }
    };
}

/// Expands the actions or subscriptions implemented locally by hostd.
///
/// Action entries contain the procedure name, input type, and output type.
/// Subscription entries contain the subscription name, input type, and event
/// type.
#[macro_export]
macro_rules! with_hostd_operations {
    (actions => $macro:ident) => {
        $macro! {
            ("auth_CreatePairingKey", auth::CreatePairingKeyAction, auth::CreatePairingKeyOutput),
            ("auth_RevokeBrowserSession", auth::RevokeBrowserSessionAction, auth::RevokeBrowserSessionOutput),
            ("auth_CreateHttpRouteTicket", auth::CreateHttpRouteTicketAction, auth::CreateHttpRouteTicketOutput),
            ("automations_Start", automations::StartAutomationAction, automations::StartAutomationOutput),
            ("automations_Cancel", automations::CancelAutomationExecutionAction, automations::CancelAutomationExecutionOutput),
            ("automations_ResolveApproval", automations::ResolveAutomationApprovalAction, automations::ResolveAutomationApprovalOutput),
            ("config_UpdateSettings", config::UpdateWorkspaceSettingsAction, config::UpdateWorkspaceSettingsOutput),
            ("config_ResolveTasciModel", config::ResolveTasciModelAction, config::ResolveTasciModelOutput),
            ("config_ResolveMcpServers", config::ResolveMcpServersAction, config::ResolveMcpServersOutput),
            ("secrets_Reveal", secrets::RevealSecretAction, secrets::RevealSecretOutput),
            ("secrets_Set", secrets::SetSecretAction, secrets::SetSecretOutput),
            ("secrets_Delete", secrets::DeleteSecretAction, secrets::DeleteSecretOutput),
            ("network_GetPodHttpRoutes", network::GetPodHttpRoutesAction, network::GetPodHttpRoutesOutput),
            ("network_GetPodPortForwards", network::GetPodPortForwardsAction, network::GetPodPortForwardsOutput),
            ("network_CreateHttpRoute", network::CreateHttpRouteAction, network::CreateHttpRouteOutput),
            ("network_SetHttpRouteTrustedTascarrelFrontend", network::SetHttpRouteTrustedTascarrelFrontendAction, network::SetHttpRouteTrustedTascarrelFrontendOutput),
            ("network_DeleteHttpRoute", network::DeleteHttpRouteAction, network::DeleteHttpRouteOutput),
            ("network_CreatePortForward", network::CreatePortForwardAction, network::CreatePortForwardOutput),
            ("network_DeletePortForward", network::DeletePortForwardAction, network::DeletePortForwardOutput),
            ("network_CreatePodHostForward", network::CreatePodHostForwardAction, network::CreatePodHostForwardOutput),
            ("network_DeletePodHostForward", network::DeletePodHostForwardAction, network::DeletePodHostForwardOutput),
            ("repositories_PrepareSnapshot", repositories::PrepareRepositorySnapshotAction, repositories::PrepareRepositorySnapshotOutput),
            ("repositories_RefreshSnapshot", repositories::RefreshRepositorySnapshotAction, repositories::RefreshRepositorySnapshotOutput),
            ("repositories_GetApprovalReview", repositories::GetRepositoryApprovalReviewAction, repositories::GetRepositoryApprovalReviewOutput),
            ("repositories_GetApprovalCommitChanges", repositories::GetRepositoryApprovalCommitChangesAction, repositories::GetRepositoryApprovalCommitChangesOutput),
            ("repositories_ResolveApproval", repositories::ResolveRepositoryApprovalAction, repositories::ResolveRepositoryApprovalOutput),
            ("hostOperations_Request", host_operations::RequestHostOperationAction, host_operations::RequestHostOperationOutput),
            ("hostOperations_Resolve", host_operations::ResolveHostOperationAction, host_operations::ResolveHostOperationOutput),
            ("hostOperations_Cancel", host_operations::CancelHostOperationAction, host_operations::CancelHostOperationOutput),
            ("shares_Inspect", shares::InspectShareOverlayAction, shares::InspectShareOverlayOutput),
            ("shares_Apply", shares::ApplyShareOverlayAction, shares::ApplyShareOverlayOutput),
            ("shares_RequestApproval", shares::RequestShareOverlayApprovalAction, shares::RequestShareOverlayApprovalOutput),
            ("shares_CancelApproval", shares::CancelShareOverlayApprovalAction, shares::CancelShareOverlayApprovalOutput),
            ("shares_ResolveApproval", shares::ResolveShareOverlayApprovalAction, shares::ResolveShareOverlayApprovalOutput),
            ("workspaces_Create", workspaces::CreateWorkspaceAction, workspaces::CreateWorkspaceOutput),
            ("workspaces_Start", workspaces::StartWorkspaceAction, workspaces::StartWorkspaceOutput),
            ("workspaces_Stop", workspaces::StopWorkspaceAction, workspaces::StopWorkspaceOutput),
            ("workspaces_Destroy", workspaces::DestroyWorkspaceAction, workspaces::DestroyWorkspaceOutput),
            ("workspaces_AttachUsbDevice", workspaces::AttachUsbDeviceAction, workspaces::AttachUsbDeviceOutput),
            ("workspaces_DetachUsbDevice", workspaces::DetachUsbDeviceAction, workspaces::DetachUsbDeviceOutput),
        }
    };
    (subscriptions => $macro:ident) => {
        $macro! {
            ("auth_BrowserSessionsChanged", auth::BrowserSessionsChangedSubscription, auth::BrowserSessionsChangedEvent),
            ("automations_Catalog", automations::AutomationCatalogSubscription, automations::AutomationCatalogEvent),
            ("automations_Executions", automations::AutomationExecutionListSubscription, automations::AutomationExecutionListEvent),
            ("automations_Output", automations::AutomationOutputSubscription, automations::AutomationOutputEvent),
            ("config_Changed", config::ConfigChangedSubscription, config::ConfigChangedEvent),
            ("secrets_Changed", secrets::SecretsChangedSubscription, secrets::SecretsChangedEvent),
            ("network_DnsRequests", network::DnsRequestsSubscription, network::DnsRequestsEvent),
            ("network_HttpRequests", network::HttpRequestsSubscription, network::HttpRequestsEvent),
            ("network_HttpRoutesChanged", network::HttpRouteListChangedSubscription, network::HttpRouteListChangedEvent),
            ("network_PortForwardsChanged", network::PortForwardListChangedSubscription, network::PortForwardListChangedEvent),
            ("network_PodHostForwardsChanged", network::PodHostForwardListChangedSubscription, network::PodHostForwardListChangedEvent),
            ("network_TcpFlows", network::TcpFlowsSubscription, network::TcpFlowsEvent),
            ("repositories_ApprovalRequestsChanged", repositories::RepositoryApprovalRequestListChangedSubscription, repositories::RepositoryApprovalRequestListChangedEvent),
            ("repositories_PushStatusChanged", repositories::RepositoryPushStatusChangedSubscription, repositories::RepositoryPushStatusChangedEvent),
            ("repositories_Changed", repositories::RepositoryListChangedSubscription, repositories::RepositoryListChangedEvent),
            ("hostOperations_Commands", host_operations::HostCommandListChangedSubscription, host_operations::HostCommandListChangedEvent),
            ("hostOperations_Changed", host_operations::HostOperationListChangedSubscription, host_operations::HostOperationListChangedEvent),
            ("hostOperations_Audit", host_operations::HostOperationAuditSubscription, host_operations::HostOperationAuditEvent),
            ("hostOperations_Output", host_operations::HostOperationOutputSubscription, host_operations::HostOperationOutputEvent),
            ("shares_ApprovalRequestsChanged", shares::ShareOverlayApprovalRequestListChangedSubscription, shares::ShareOverlayApprovalRequestListChangedEvent),
            ("workspaces_Changed", workspaces::WorkspaceListChangedSubscription, workspaces::WorkspaceListChangedEvent),
            ("workspaces_VmLog", workspaces::WorkspaceVmLogSubscription, workspaces::WorkspaceVmLogEvent),
            ("workspaces_UsbDevicesChanged", workspaces::UsbDevicesChangedSubscription, workspaces::UsbDevicesChangedEvent),
        }
    };
}

/// Expands to every supported action and its Sidex input/output types.
#[macro_export]
macro_rules! with_all_actions {
    ($macro:ident) => {
        $crate::__with_guestd_actions! {
            @append $macro {
                ("auth_CreatePairingKey", auth::CreatePairingKeyAction, auth::CreatePairingKeyOutput),
                ("auth_RevokeBrowserSession", auth::RevokeBrowserSessionAction, auth::RevokeBrowserSessionOutput),
                ("auth_CreateHttpRouteTicket", auth::CreateHttpRouteTicketAction, auth::CreateHttpRouteTicketOutput),
                ("automations_Start", automations::StartAutomationAction, automations::StartAutomationOutput),
                ("automations_Cancel", automations::CancelAutomationExecutionAction, automations::CancelAutomationExecutionOutput),
                ("automations_ResolveApproval", automations::ResolveAutomationApprovalAction, automations::ResolveAutomationApprovalOutput),
                ("config_UpdateSettings", config::UpdateWorkspaceSettingsAction, config::UpdateWorkspaceSettingsOutput),
                ("config_ResolveTasciModel", config::ResolveTasciModelAction, config::ResolveTasciModelOutput),
                ("config_ResolveMcpServers", config::ResolveMcpServersAction, config::ResolveMcpServersOutput),
                ("secrets_Reveal", secrets::RevealSecretAction, secrets::RevealSecretOutput),
                ("secrets_Set", secrets::SetSecretAction, secrets::SetSecretOutput),
                ("secrets_Delete", secrets::DeleteSecretAction, secrets::DeleteSecretOutput),
                ("network_GetPodHttpRoutes", network::GetPodHttpRoutesAction, network::GetPodHttpRoutesOutput),
                ("network_GetPodPortForwards", network::GetPodPortForwardsAction, network::GetPodPortForwardsOutput),
                ("network_CreateHttpRoute", network::CreateHttpRouteAction, network::CreateHttpRouteOutput),
                ("network_SetHttpRouteTrustedTascarrelFrontend", network::SetHttpRouteTrustedTascarrelFrontendAction, network::SetHttpRouteTrustedTascarrelFrontendOutput),
                ("network_DeleteHttpRoute", network::DeleteHttpRouteAction, network::DeleteHttpRouteOutput),
                ("network_CreatePortForward", network::CreatePortForwardAction, network::CreatePortForwardOutput),
                ("network_DeletePortForward", network::DeletePortForwardAction, network::DeletePortForwardOutput),
                ("network_CreatePodHostForward", network::CreatePodHostForwardAction, network::CreatePodHostForwardOutput),
                ("network_DeletePodHostForward", network::DeletePodHostForwardAction, network::DeletePodHostForwardOutput),
                ("repositories_PrepareSnapshot", repositories::PrepareRepositorySnapshotAction, repositories::PrepareRepositorySnapshotOutput),
                ("repositories_RefreshSnapshot", repositories::RefreshRepositorySnapshotAction, repositories::RefreshRepositorySnapshotOutput),
                ("repositories_GetApprovalReview", repositories::GetRepositoryApprovalReviewAction, repositories::GetRepositoryApprovalReviewOutput),
                ("repositories_GetApprovalCommitChanges", repositories::GetRepositoryApprovalCommitChangesAction, repositories::GetRepositoryApprovalCommitChangesOutput),
                ("repositories_ResolveApproval", repositories::ResolveRepositoryApprovalAction, repositories::ResolveRepositoryApprovalOutput),
                ("hostOperations_Request", host_operations::RequestHostOperationAction, host_operations::RequestHostOperationOutput),
                ("hostOperations_Resolve", host_operations::ResolveHostOperationAction, host_operations::ResolveHostOperationOutput),
                ("hostOperations_Cancel", host_operations::CancelHostOperationAction, host_operations::CancelHostOperationOutput),
                ("shares_Inspect", shares::InspectShareOverlayAction, shares::InspectShareOverlayOutput),
                ("shares_Apply", shares::ApplyShareOverlayAction, shares::ApplyShareOverlayOutput),
                ("shares_RequestApproval", shares::RequestShareOverlayApprovalAction, shares::RequestShareOverlayApprovalOutput),
                ("shares_CancelApproval", shares::CancelShareOverlayApprovalAction, shares::CancelShareOverlayApprovalOutput),
                ("shares_ResolveApproval", shares::ResolveShareOverlayApprovalAction, shares::ResolveShareOverlayApprovalOutput),
                ("workspaces_Create", workspaces::CreateWorkspaceAction, workspaces::CreateWorkspaceOutput),
                ("workspaces_Start", workspaces::StartWorkspaceAction, workspaces::StartWorkspaceOutput),
                ("workspaces_Stop", workspaces::StopWorkspaceAction, workspaces::StopWorkspaceOutput),
                ("workspaces_Destroy", workspaces::DestroyWorkspaceAction, workspaces::DestroyWorkspaceOutput),
                ("workspaces_AttachUsbDevice", workspaces::AttachUsbDeviceAction, workspaces::AttachUsbDeviceOutput),
                ("workspaces_DetachUsbDevice", workspaces::DetachUsbDeviceAction, workspaces::DetachUsbDeviceOutput),
            }
        }
    };
}

/// Expands to every supported WebSocket subscription and its event type.
#[macro_export]
macro_rules! with_all_subscriptions {
    ($macro:ident) => {
        $crate::__with_guestd_subscriptions! {
            @append $macro {
                ("auth_BrowserSessionsChanged", auth::BrowserSessionsChangedSubscription, auth::BrowserSessionsChangedEvent),
                ("automations_Catalog", automations::AutomationCatalogSubscription, automations::AutomationCatalogEvent),
                ("automations_Executions", automations::AutomationExecutionListSubscription, automations::AutomationExecutionListEvent),
                ("automations_Output", automations::AutomationOutputSubscription, automations::AutomationOutputEvent),
                ("config_Changed", config::ConfigChangedSubscription, config::ConfigChangedEvent),
                ("secrets_Changed", secrets::SecretsChangedSubscription, secrets::SecretsChangedEvent),
                ("network_DnsRequests", network::DnsRequestsSubscription, network::DnsRequestsEvent),
                ("network_HttpRequests", network::HttpRequestsSubscription, network::HttpRequestsEvent),
                ("network_HttpRoutesChanged", network::HttpRouteListChangedSubscription, network::HttpRouteListChangedEvent),
                ("network_PortForwardsChanged", network::PortForwardListChangedSubscription, network::PortForwardListChangedEvent),
                ("network_PodHostForwardsChanged", network::PodHostForwardListChangedSubscription, network::PodHostForwardListChangedEvent),
                ("network_TcpFlows", network::TcpFlowsSubscription, network::TcpFlowsEvent),
                ("repositories_ApprovalRequestsChanged", repositories::RepositoryApprovalRequestListChangedSubscription, repositories::RepositoryApprovalRequestListChangedEvent),
                ("repositories_PushStatusChanged", repositories::RepositoryPushStatusChangedSubscription, repositories::RepositoryPushStatusChangedEvent),
                ("repositories_Changed", repositories::RepositoryListChangedSubscription, repositories::RepositoryListChangedEvent),
                ("hostOperations_Commands", host_operations::HostCommandListChangedSubscription, host_operations::HostCommandListChangedEvent),
                ("hostOperations_Changed", host_operations::HostOperationListChangedSubscription, host_operations::HostOperationListChangedEvent),
                ("hostOperations_Audit", host_operations::HostOperationAuditSubscription, host_operations::HostOperationAuditEvent),
                ("hostOperations_Output", host_operations::HostOperationOutputSubscription, host_operations::HostOperationOutputEvent),
                ("shares_ApprovalRequestsChanged", shares::ShareOverlayApprovalRequestListChangedSubscription, shares::ShareOverlayApprovalRequestListChangedEvent),
                ("workspaces_Changed", workspaces::WorkspaceListChangedSubscription, workspaces::WorkspaceListChangedEvent),
                ("workspaces_VmLog", workspaces::WorkspaceVmLogSubscription, workspaces::WorkspaceVmLogEvent),
                ("workspaces_UsbDevicesChanged", workspaces::UsbDevicesChangedSubscription, workspaces::UsbDevicesChangedEvent),
            }
        }
    };
}

pub trait Action:
    'static + Sized + Clone + Send + Sync + serde::Serialize + serde::de::DeserializeOwned
{
    type Output: 'static
        + Sized
        + Clone
        + Send
        + Sync
        + serde::Serialize
        + serde::de::DeserializeOwned;

    const NAME: &'static str;
}

pub trait Subscription:
    'static + Sized + Clone + Send + Sync + serde::Serialize + serde::de::DeserializeOwned
{
    type Event: 'static
        + Sized
        + Clone
        + Send
        + Sync
        + serde::Serialize
        + serde::de::DeserializeOwned;

    const NAME: &'static str;
}

/// Typed action implemented by a workspace guest daemon.
///
/// This marker restricts generic clients to procedures present in the guestd
/// action registry.
pub trait GuestAction: Action + guest_operation_seal::Action {}

/// Typed subscription implemented by a workspace guest daemon.
///
/// This marker restricts generic clients to subscriptions present in the
/// guestd subscription registry.
pub trait GuestSubscription: Subscription + guest_operation_seal::Subscription {}

/// Typed action implemented locally by the host daemon.
///
/// This marker restricts host-local dispatch to actions present in the hostd
/// action registry.
pub trait HostAction: Action + host_operation_seal::Action {}

/// Typed subscription implemented locally by the host daemon.
///
/// This marker restricts host-local dispatch to subscriptions present in the
/// hostd subscription registry.
pub trait HostSubscription: Subscription + host_operation_seal::Subscription {}

/// Prevents operation types outside the registry from entering typed guestd
/// clients.
mod guest_operation_seal {
    /// Seals the public guest action marker.
    pub trait Action {}
    /// Seals the public guest subscription marker.
    pub trait Subscription {}
}

/// Prevents operation types outside the registry from entering typed hostd
/// dispatch.
mod host_operation_seal {
    /// Seals the public host action marker.
    pub trait Action {}
    /// Seals the public host subscription marker.
    pub trait Subscription {}
}

mod action_impls {
    use crate::types::auth;
    use crate::types::automations;
    use crate::types::changes;
    use crate::types::chats;
    use crate::types::code;
    use crate::types::config;
    use crate::types::files;
    use crate::types::guest;
    use crate::types::host_operations;
    use crate::types::images;
    use crate::types::network;
    use crate::types::pods;
    use crate::types::processes;
    use crate::types::repositories;
    use crate::types::secrets;
    use crate::types::shares;
    use crate::types::workspaces;

    macro_rules! implement {
        ($(($name:literal, $input:path, $output:path),)*) => {
            $(
                impl crate::Action for $input {
                    type Output = $output;
                    const NAME: &'static str = $name;
                }
            )*
        };
    }

    crate::with_all_actions!(implement);
}

mod subscription_impls {
    use crate::types::auth;
    use crate::types::automations;
    use crate::types::changes;
    use crate::types::chats;
    use crate::types::code;
    use crate::types::config;
    use crate::types::guest;
    use crate::types::host_operations;
    use crate::types::images;
    use crate::types::network;
    use crate::types::pods;
    use crate::types::processes;
    use crate::types::repositories;
    use crate::types::secrets;
    use crate::types::shares;
    use crate::types::workspaces;

    macro_rules! implement {
        ($(($name:literal, $input:path, $output:path),)*) => {
            $(
                impl crate::Subscription for $input {
                    type Event = $output;
                    const NAME: &'static str = $name;
                }
            )*
        };
    }

    crate::with_all_subscriptions!(implement);
}

mod guest_operation_impls {
    use crate::types::changes;
    use crate::types::chats;
    use crate::types::code;
    use crate::types::files;
    use crate::types::guest;
    use crate::types::images;
    use crate::types::pods;
    use crate::types::processes;

    macro_rules! implement_actions {
        ($(($name:literal, $input:path, $output:path),)*) => {
            $(
                impl super::guest_operation_seal::Action for $input {}
                impl crate::GuestAction for $input {}
            )*
        };
    }

    macro_rules! implement_subscriptions {
        ($(($name:literal, $input:path, $event:path),)*) => {
            $(
                impl super::guest_operation_seal::Subscription for $input {}
                impl crate::GuestSubscription for $input {}
            )*
        };
    }

    crate::with_guestd_operations!(actions => implement_actions);
    crate::with_guestd_operations!(subscriptions => implement_subscriptions);
}

mod host_operation_impls {
    use crate::types::auth;
    use crate::types::automations;
    use crate::types::config;
    use crate::types::host_operations;
    use crate::types::network;
    use crate::types::repositories;
    use crate::types::secrets;
    use crate::types::shares;
    use crate::types::workspaces;

    macro_rules! implement_actions {
        ($(($name:literal, $input:path, $output:path),)*) => {
            $(
                impl super::host_operation_seal::Action for $input {}
                impl crate::HostAction for $input {}
            )*
        };
    }

    macro_rules! implement_subscriptions {
        ($(($name:literal, $input:path, $event:path),)*) => {
            $(
                impl super::host_operation_seal::Subscription for $input {}
                impl crate::HostSubscription for $input {}
            )*
        };
    }

    crate::with_hostd_operations!(actions => implement_actions);
    crate::with_hostd_operations!(subscriptions => implement_subscriptions);
}

#[cfg(test)]
mod tests {
    use crate::types::protocol::OperationError;
    use crate::types::protocol::OperationErrorDetails;

    /// A relayed operation failure displays the complete peer report instead
    /// of discarding its causal diagnostics.
    #[test]
    fn operation_error_displays_peer_report() {
        let error = OperationError::Unavailable(OperationErrorDetails {
            message: "repository refresh failed".into(),
            report: Some("repository refresh failed\ncause: ssh is unavailable".into()),
        });

        assert_eq!(
            error.to_string(),
            "repository refresh failed\ncause: ssh is unavailable"
        );
    }
}
