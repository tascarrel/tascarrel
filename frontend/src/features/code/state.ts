import { guestApi } from "../../api/client.ts";
import type { code, store, workspaces } from "../../api/generated/index.ts";
import type { BackendStateDefinition } from "../../shared/state/BackendStateCache.ts";
import { useBackendState } from "../../shared/state/StateCacheProvider.tsx";
import { applyStoreEvent } from "../../shared/state/storeEvents.ts";

export function useCodeSessions(workspace: workspaces.WorkspaceName) {
  return useBackendState(codeSessionListDefinition(workspace));
}

function codeSessionListDefinition(
  workspace: workspaces.WorkspaceName,
): BackendStateDefinition<code.CodeSessionList, code.CodeSessionListChangedEvent, store.Stamp> {
  return {
    key: `guest/${workspace}/code/sessions`,
    retention: "lru",
    connect: (cursor, handlers) => guestApi(workspace).subscribe(
      "code_Changed",
      () => ({ workspace, ...(cursor() ? { cursor: cursor() } : {}) }),
      {
        onEvent: handlers.onEvent,
        onState: handlers.onConnection,
        onError: handlers.onError,
      },
    ),
    applyEvent: (current, event) => applyStoreEvent(
      current,
      event.change,
      applyCodeSessionMutation,
    ),
  };
}

function applyCodeSessionMutation(
  list: code.CodeSessionList,
  mutation: code.CodeSessionListMutation,
): code.CodeSessionList {
  if (mutation.type === "Remove") {
    return {
      codeSessions: list.codeSessions.filter((session) => session.id !== mutation.content),
    };
  }
  const session: code.CodeSession = {
    id: mutation.id,
    workspace: mutation.workspace,
    podId: mutation.podId,
    folder: mutation.folder,
    title: mutation.title,
    podPort: mutation.podPort,
    processId: mutation.processId,
    httpRouteId: mutation.httpRouteId,
    hostnamePrefix: mutation.hostnamePrefix,
    status: mutation.status,
  };
  const index = list.codeSessions.findIndex((candidate) => candidate.id === session.id);
  return {
    codeSessions: index < 0
      ? [...list.codeSessions, session].toSorted((left, right) => left.id.localeCompare(right.id))
      : list.codeSessions.map((candidate, candidateIndex) =>
          candidateIndex === index ? session : candidate
        ),
  };
}
