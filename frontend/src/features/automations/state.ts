import { hostApi } from "../../api/client.ts";
import type { automations, workspaces } from "../../api/generated/index.ts";
import type { BackendStateDefinition } from "../../shared/state/BackendStateCache.ts";
import { useBackendState } from "../../shared/state/StateCacheProvider.tsx";

export function useAutomationCatalog(workspace: workspaces.WorkspaceName) {
  return useBackendState(automationCatalogDefinition(workspace));
}

export function useAutomationExecutions(workspace: workspaces.WorkspaceName) {
  return useBackendState(automationExecutionListDefinition(workspace));
}

function automationCatalogDefinition(
  workspace: workspaces.WorkspaceName,
): BackendStateDefinition<
  automations.AutomationCatalog,
  automations.AutomationCatalogEvent,
  undefined
> {
  return {
    key: `host/automations/catalog/${workspace}`,
    connect: (_cursor, handlers) =>
      hostApi.subscribe(
        "automations_Catalog",
        { workspace },
        {
          onEvent: handlers.onEvent,
          onState: handlers.onConnection,
          onError: handlers.onError,
        },
      ),
    applyEvent: (_current, event) => ({ value: event.value }),
  };
}

function automationExecutionListDefinition(
  workspace: workspaces.WorkspaceName,
): BackendStateDefinition<
  automations.AutomationExecutionList,
  automations.AutomationExecutionListEvent,
  automations.AutomationRevision
> {
  return {
    key: `host/automations/executions/${workspace}`,
    connect: (cursor, handlers) =>
      hostApi.subscribe(
        "automations_Executions",
        () => {
          const revision = cursor();
          return {
            workspace,
            ...(revision === undefined ? {} : { cursor: revision }),
          };
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
