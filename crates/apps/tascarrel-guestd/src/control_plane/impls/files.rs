//! Control-plane implementation for workspace directory inspection.

use async_trait::async_trait;
use reportify::Report;
use tascarrel_api::types::files as api;
use tascarrel_api::types::protocol as wire;

use crate::control_plane::InvocationCtx;
use crate::control_plane::operation_error_details;
use crate::control_plane::operations::ExecuteAction;
use crate::services::files::FilesServiceError;

#[async_trait]
impl ExecuteAction for api::ListRootsAction {
    fn check_permissions(
        &self,
        context: &InvocationCtx<'_>,
    ) -> Result<(), Report<wire::OperationError>> {
        check_permissions(context)
    }

    async fn execute(
        self,
        context: InvocationCtx<'_>,
    ) -> Result<Self::Output, Report<wire::OperationError>> {
        context
            .state()
            .files()
            .list_roots(&self.pod_id, context.state().pods())
            .await
            .map_err(files_error)
    }
}

#[async_trait]
impl ExecuteAction for api::ReadDirectoryAction {
    fn check_permissions(
        &self,
        context: &InvocationCtx<'_>,
    ) -> Result<(), Report<wire::OperationError>> {
        check_permissions(context)
    }

    async fn execute(
        self,
        context: InvocationCtx<'_>,
    ) -> Result<Self::Output, Report<wire::OperationError>> {
        if matches!(&self.root, None | Some(api::FileRoot::Workspace)) {
            context
                .state()
                .changes()
                .ensure_tracking(
                    context.state().pods().clone(),
                    context.state().repositories().cloned(),
                    context.state().repository_config().cloned(),
                )
                .await;
        }
        context
            .state()
            .files()
            .read_directory(self, context.state().pods(), context.state().changes())
            .await
            .map_err(files_error)
    }
}

fn check_permissions(context: &InvocationCtx<'_>) -> Result<(), Report<wire::OperationError>> {
    if context
        .require_routing_context()?
        .caller
        .is_host_or_client()
    {
        Ok(())
    } else {
        Err(wire::OperationError::forbidden())
    }
}

fn files_error(report: Report<FilesServiceError>) -> Report<wire::OperationError> {
    let error = match report.error() {
        FilesServiceError::InvalidRequest(message) => {
            wire::OperationError::InvalidRequest(operation_error_details(message.clone()))
        }
        FilesServiceError::ReadOnly => {
            wire::OperationError::InvalidRequest(operation_error_details("file root is read-only"))
        }
        FilesServiceError::Conflict => wire::OperationError::Unavailable(operation_error_details(
            "file changed since it was read",
        )),
        FilesServiceError::Unavailable(message) => {
            wire::OperationError::Unavailable(operation_error_details(message.clone()))
        }
        FilesServiceError::Internal(message) => {
            wire::OperationError::Internal(operation_error_details(message.clone()))
        }
    };
    report.escalate(error)
}
