import { hostApi } from "../../api/client.ts";
import type { host_operations, workspaces } from "../../api/generated/index.ts";
import type { BackendStateDefinition } from "../../shared/state/BackendStateCache.ts";
import { useBackendState } from "../../shared/state/StateCacheProvider.tsx";

export function useHostOperations(workspace: workspaces.WorkspaceName) {
  return useBackendState(hostOperationListDefinition(workspace));
}

function hostOperationListDefinition(
  workspace: workspaces.WorkspaceName,
): BackendStateDefinition<
  host_operations.HostOperationList,
  host_operations.HostOperationListChangedEvent,
  host_operations.HostOperationRevision
> {
  return {
    key: `host/operations/${workspace}`,
    connect: (cursor, handlers) => hostApi.subscribe(
      "hostOperations_Changed",
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
