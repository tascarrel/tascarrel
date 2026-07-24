import { hostApi } from "../../api/client.ts";
import type { store, workspaces } from "../../api/generated/index.ts";
import type { BackendStateDefinition } from "../../shared/state/BackendStateCache.ts";
import { useBackendState } from "../../shared/state/StateCacheProvider.tsx";
import { applyStoreEvent } from "../../shared/state/storeEvents.ts";

const WORKSPACE_LIST_KEY = "host/workspaces";

export function useWorkspaces() {
  return useBackendState(workspaceListDefinition());
}

function workspaceListDefinition(): BackendStateDefinition<
  workspaces.WorkspaceList,
  workspaces.WorkspaceListChangedEvent,
  store.Stamp
> {
  return {
    key: WORKSPACE_LIST_KEY,
    connect: (cursor, handlers) => hostApi.subscribe(
      "workspaces_Changed",
      () => cursor() ? { cursor: cursor() } : {},
      {
        onEvent: handlers.onEvent,
        onState: handlers.onConnection,
        onError: handlers.onError,
      },
    ),
    applyEvent: (current, event) => applyStoreEvent(
      current,
      event.change,
      (list, mutation) => {
        if (mutation.type === "Remove") {
          return {
            workspaces: list.workspaces.filter((workspace) => workspace.name !== mutation.content),
          };
        }
        const index = list.workspaces.findIndex((workspace) => workspace.name === mutation.name);
        return {
          workspaces: index < 0
            ? [...list.workspaces, mutation]
            : list.workspaces.map((workspace, candidateIndex) =>
                candidateIndex === index ? mutation : workspace
              ),
        };
      },
    ),
  };
}
