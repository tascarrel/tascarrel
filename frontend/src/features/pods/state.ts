import { guestApi } from "../../api/client.ts";
import type { pods, store, workspaces } from "../../api/generated/index.ts";
import type { BackendStateDefinition } from "../../shared/state/BackendStateCache.ts";
import { useBackendState } from "../../shared/state/StateCacheProvider.tsx";
import { applyStoreEvent } from "../../shared/state/storeEvents.ts";

export type PodListState = pods.PodList & Readonly<{
  podTitlesById: ReadonlyMap<pods.PodId, string>;
}>;

export function usePods(workspace: workspaces.WorkspaceName) {
  return useBackendState(podListDefinition(workspace));
}

function podListDefinition(
  workspace: workspaces.WorkspaceName,
): BackendStateDefinition<PodListState, pods.PodListChangedEvent, store.Stamp> {
  return {
    key: `guest/${workspace}/pods`,
    connect: (cursor, handlers) => guestApi(workspace).subscribe(
      "pods_Changed",
      () => cursor() ? { cursor: cursor() } : {},
      {
        onEvent: handlers.onEvent,
        onState: handlers.onConnection,
        onError: handlers.onError,
      },
    ),
    applyEvent: (current, event) => {
      const next = applyStoreEvent<pods.PodList, pods.PodListMutation>(
        current,
        event.change,
        (list, mutation) => {
          if (mutation.type === "Remove") {
            return { pods: list.pods.filter((pod) => pod.id !== mutation.content) };
          }
          const index = list.pods.findIndex((pod) => pod.id === mutation.id);
          return {
            pods: index < 0
              ? [...list.pods, mutation]
              : list.pods.map((pod, candidateIndex) => candidateIndex === index ? mutation : pod),
          };
        },
      );
      return {
        ...next,
        value: withPodTitleIndex(next.value, current?.podTitlesById),
      };
    },
  };
}

function withPodTitleIndex(
  list: pods.PodList,
  previous?: ReadonlyMap<pods.PodId, string>,
): PodListState {
  const podTitlesById = new Map(previous);
  for (const pod of list.pods) {
    podTitlesById.set(pod.id, pod.title || "Untitled pod");
  }
  return {
    ...list,
    podTitlesById,
  };
}
