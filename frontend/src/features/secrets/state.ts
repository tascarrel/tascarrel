import { hostApi } from "../../api/client.ts";
import type { secrets, workspaces } from "../../api/generated/index.ts";
import type { BackendStateDefinition } from "../../shared/state/BackendStateCache.ts";
import { useBackendState } from "../../shared/state/StateCacheProvider.tsx";

export function useWorkspaceSecrets(workspace: workspaces.WorkspaceName) {
  return useBackendState(workspaceSecretsDefinition(workspace));
}

function workspaceSecretsDefinition(
  workspace: workspaces.WorkspaceName,
): BackendStateDefinition<secrets.SecretsChangedEvent, secrets.SecretsChangedEvent, never> {
  return {
    key: `host/secrets/${workspace}`,
    connect: (_cursor, handlers) => hostApi.subscribe(
      "secrets_Changed",
      { workspaceName: workspace },
      {
        onEvent: handlers.onEvent,
        onState: handlers.onConnection,
        onError: handlers.onError,
      },
    ),
    applyEvent: (_current, event) => ({ value: event }),
  };
}
