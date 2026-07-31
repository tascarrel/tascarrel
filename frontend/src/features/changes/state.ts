import { guestApi, hostApi } from "../../api/client.ts";
import type { changes, shares, store, workspaces } from "../../api/generated/index.ts";
import type { BackendStateDefinition } from "../../shared/state/BackendStateCache.ts";
import { useBackendState } from "../../shared/state/StateCacheProvider.tsx";
import { applyStoreEvent } from "../../shared/state/storeEvents.ts";

export function useRepositoryStatuses(workspace: workspaces.WorkspaceName) {
  return useBackendState(repositoryStatusDefinition(workspace));
}

export function useShareOverlayApprovals(workspace: workspaces.WorkspaceName) {
  return useBackendState(shareOverlayApprovalDefinition(workspace));
}

function shareOverlayApprovalDefinition(
  workspace: workspaces.WorkspaceName,
): BackendStateDefinition<
  shares.ShareOverlayApprovalRequestList,
  shares.ShareOverlayApprovalRequestListChangedEvent,
  shares.ShareOverlayApprovalListRevision
> {
  return {
    key: `host/shares/${workspace}/approvals`,
    connect: (cursor, handlers) => hostApi.subscribe(
      "shares_ApprovalRequestsChanged",
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
