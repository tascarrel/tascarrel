import { hostApi } from "../../api/client.ts";
import type { repositories, workspaces } from "../../api/generated/index.ts";
import type { BackendStateDefinition } from "../../shared/state/BackendStateCache.ts";
import { useBackendState } from "../../shared/state/StateCacheProvider.tsx";

export function useRepositories(workspace: workspaces.WorkspaceName) {
  return useBackendState(repositoryListDefinition(workspace));
}

export function useRepositoryApprovals(workspace: workspaces.WorkspaceName) {
  return useBackendState(repositoryApprovalListDefinition(workspace));
}

function repositoryListDefinition(
  workspace: workspaces.WorkspaceName,
): BackendStateDefinition<
  repositories.RepositoryList,
  repositories.RepositoryListChangedEvent,
  repositories.RepositoryRevision
> {
  return {
    key: `host/repositories/${workspace}`,
    connect: (cursor, handlers) => hostApi.subscribe(
      "repositories_Changed",
      () => {
        const revision = cursor();
        return { workspace, ...(revision === undefined ? {} : { cursor: revision }) };
      },
      {
        onEvent: handlers.onEvent,
        onState: handlers.onConnection,
        onError: handlers.onError,
      },
    ),
    applyEvent: (_current, event) => ({
      value: event.value,
      cursor: event.revision,
    }),
  };
}

function repositoryApprovalListDefinition(
  workspace: workspaces.WorkspaceName,
): BackendStateDefinition<
  repositories.RepositoryApprovalRequestList,
  repositories.RepositoryApprovalRequestListChangedEvent,
  repositories.RepositoryRevision
> {
  return {
    key: `host/repositories/${workspace}/approvals`,
    connect: (cursor, handlers) => hostApi.subscribe(
      "repositories_ApprovalRequestsChanged",
      () => {
        const revision = cursor();
        return { workspace, ...(revision === undefined ? {} : { cursor: revision }) };
      },
      {
        onEvent: handlers.onEvent,
        onState: handlers.onConnection,
        onError: handlers.onError,
      },
    ),
    applyEvent: (_current, event) => ({
      value: event.value,
      cursor: event.revision,
    }),
  };
}
