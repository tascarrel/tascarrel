import { guestApi } from "../../api/client.ts";
import type { changes, store, workspaces } from "../../api/generated/index.ts";
import type { BackendStateDefinition } from "../../shared/state/BackendStateCache.ts";
import { useBackendState } from "../../shared/state/StateCacheProvider.tsx";
import { applyStoreEvent } from "../../shared/state/storeEvents.ts";

export function useRepositoryStatuses(workspace: workspaces.WorkspaceName) {
  return useBackendState(repositoryStatusDefinition(workspace));
}

function repositoryStatusDefinition(
  workspace: workspaces.WorkspaceName,
): BackendStateDefinition<
  changes.RepositoryStatusList,
  changes.RepositoryStatusListChangedEvent,
  store.Stamp
> {
  return {
    key: `guest/${workspace}/repository-statuses`,
    connect: (cursor, handlers) => guestApi(workspace).subscribe(
      "changes_Changed",
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
            repositories: list.repositories.filter((entry) =>
              !sameTarget(entry.target, mutation)
            ),
          };
        }
        const index = list.repositories.findIndex((entry) =>
          sameTarget(entry.target, mutation.target)
        );
        const repositories = index < 0
          ? [...list.repositories, mutation]
          : list.repositories.map((entry, candidate) => candidate === index ? mutation : entry);
        return {
          repositories: repositories.toSorted((left, right) =>
            String(left.target.podId).localeCompare(String(right.target.podId))
              || String(left.target.path).localeCompare(String(right.target.path))
          ),
        };
      },
    ),
  };
}

function sameTarget(left: changes.RepositoryTarget, right: changes.RepositoryTarget): boolean {
  return left.podId === right.podId && left.path === right.path;
}
