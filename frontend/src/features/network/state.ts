import { hostApi } from "../../api/client.ts";
import type { network, store, workspaces } from "../../api/generated/index.ts";
import type { BackendStateDefinition } from "../../shared/state/BackendStateCache.ts";
import { useBackendState } from "../../shared/state/StateCacheProvider.tsx";
import { applyStoreEvent } from "../../shared/state/storeEvents.ts";

export function useHttpRoutes(workspace: workspaces.WorkspaceName) {
  return useBackendState(httpRouteListDefinition(workspace));
}

export function usePortForwards(workspace: workspaces.WorkspaceName) {
  return useBackendState(portForwardListDefinition(workspace));
}

export function usePodHostForwards(workspace: workspaces.WorkspaceName) {
  return useBackendState(podHostForwardListDefinition(workspace));
}

export function useDnsRequests(workspace: workspaces.WorkspaceName) {
  return useBackendState(dnsRequestsDefinition(workspace));
}

export function useHttpRequests(workspace: workspaces.WorkspaceName) {
  return useBackendState(httpRequestsDefinition(workspace));
}

export function useTcpFlows(workspace: workspaces.WorkspaceName) {
  return useBackendState(tcpFlowsDefinition(workspace));
}

export type DnsRequestReplica = Readonly<{
  requests: readonly network.DnsRequest[];
  hostInstanceId: network.DnsRequestCursor["hostInstanceId"];
  lastPosition: bigint;
}>;

export type HttpRequestReplica = Readonly<{
  requests: readonly network.MediatedHttpRequest[];
  hostInstanceId: network.HttpRequestCursor["hostInstanceId"];
  lastPosition: bigint;
}>;

export type TcpFlowReplica = Readonly<{
  events: readonly network.TcpFlowEvent[];
  hostInstanceId: network.TcpFlowCursor["hostInstanceId"];
  lastPosition: bigint;
}>;

const DNS_REQUEST_LIMIT = 512;
const HTTP_REQUEST_LIMIT = 512;
const TCP_FLOW_EVENT_LIMIT = 1_024;

function httpRouteListDefinition(
  workspace: workspaces.WorkspaceName,
): BackendStateDefinition<network.HttpRouteList, network.HttpRouteListChangedEvent, store.Stamp> {
  return {
    key: `host/network/${workspace}/http-routes`,
    connect: (cursor, handlers) => hostApi.subscribe(
      "network_HttpRoutesChanged",
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
      applyHttpRouteMutation,
    ),
  };
}

function portForwardListDefinition(
  workspace: workspaces.WorkspaceName,
): BackendStateDefinition<network.PortForwardList, network.PortForwardListChangedEvent, store.Stamp> {
  return {
    key: `host/network/${workspace}/port-forwards`,
    connect: (cursor, handlers) => hostApi.subscribe(
      "network_PortForwardsChanged",
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
      applyPortForwardMutation,
    ),
  };
}

function podHostForwardListDefinition(
  workspace: workspaces.WorkspaceName,
): BackendStateDefinition<network.PodHostForwardList, network.PodHostForwardListChangedEvent, store.Stamp> {
  return {
    key: `host/network/${workspace}/pod-host-forwards`,
    connect: (cursor, handlers) => hostApi.subscribe(
      "network_PodHostForwardsChanged",
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
      applyPodHostForwardMutation,
    ),
  };
}

function dnsRequestsDefinition(
  workspace: workspaces.WorkspaceName,
): BackendStateDefinition<DnsRequestReplica, network.DnsRequestsEvent, network.DnsRequestCursor> {
  return {
    key: `host/network/${workspace}/dns-requests`,
    retention: "lru",
    connect: (cursor, handlers) => hostApi.subscribe(
      "network_DnsRequests",
      () => ({ workspace, ...(cursor() ? { cursor: cursor() } : {}) }),
      {
        onEvent: handlers.onEvent,
        onState: handlers.onConnection,
        onError: handlers.onError,
      },
    ),
    applyEvent: (current, event) => {
      const batch = appendActivityBatch(
        current && {
          entries: current.requests,
          hostInstanceId: current.hostInstanceId,
          lastPosition: current.lastPosition,
        },
        event.requests,
        event.cursor,
        DNS_REQUEST_LIMIT,
      );
      const { entries, ...stream } = batch;
      return {
        value: { requests: entries, ...stream },
        cursor: event.cursor,
      };
    },
  };
}

function httpRequestsDefinition(
  workspace: workspaces.WorkspaceName,
): BackendStateDefinition<HttpRequestReplica, network.HttpRequestsEvent, network.HttpRequestCursor> {
  return {
    key: `host/network/${workspace}/http-requests`,
    retention: "lru",
    connect: (cursor, handlers) => hostApi.subscribe(
      "network_HttpRequests",
      () => ({ workspace, ...(cursor() ? { cursor: cursor() } : {}) }),
      {
        onEvent: handlers.onEvent,
        onState: handlers.onConnection,
        onError: handlers.onError,
      },
    ),
    applyEvent: (current, event) => {
      const batch = appendActivityBatch(
        current && {
          entries: current.requests,
          hostInstanceId: current.hostInstanceId,
          lastPosition: current.lastPosition,
        },
        event.requests,
        event.cursor,
        HTTP_REQUEST_LIMIT,
      );
      const { entries, ...stream } = batch;
      return {
        value: { requests: entries, ...stream },
        cursor: event.cursor,
      };
    },
  };
}

function tcpFlowsDefinition(
  workspace: workspaces.WorkspaceName,
): BackendStateDefinition<TcpFlowReplica, network.TcpFlowsEvent, network.TcpFlowCursor> {
  return {
    key: `host/network/${workspace}/tcp-flows`,
    retention: "lru",
    connect: (cursor, handlers) => hostApi.subscribe(
      "network_TcpFlows",
      () => ({ workspace, ...(cursor() ? { cursor: cursor() } : {}) }),
      {
        onEvent: handlers.onEvent,
        onState: handlers.onConnection,
        onError: handlers.onError,
      },
    ),
    applyEvent: (current, event) => {
      const batch = appendActivityBatch(
        current && {
          entries: current.events,
          hostInstanceId: current.hostInstanceId,
          lastPosition: current.lastPosition,
        },
        event.events,
        event.cursor,
        TCP_FLOW_EVENT_LIMIT,
      );
      const { entries, ...stream } = batch;
      return {
        value: { events: entries, ...stream },
        cursor: event.cursor,
      };
    },
  };
}

function appendActivityBatch<T, C extends { hostInstanceId: string; position: string | number }>(
  current: Readonly<{
    entries: readonly T[];
    hostInstanceId: C["hostInstanceId"];
    lastPosition: bigint;
  }> | undefined,
  incoming: readonly T[],
  cursor: C,
  limit: number,
): Readonly<{
  entries: readonly T[];
  hostInstanceId: C["hostInstanceId"];
  lastPosition: bigint;
}> {
  const lastPosition = BigInt(String(cursor.position));
  const firstPosition = lastPosition - BigInt(incoming.length) + 1n;
  const contiguous = current?.hostInstanceId === cursor.hostInstanceId
    && firstPosition === current.lastPosition + 1n;
  const retained = contiguous ? current.entries : [];
  const entries = incoming.length >= limit
    ? incoming.slice(-limit)
    : [...retained, ...incoming].slice(-limit);
  return { entries, hostInstanceId: cursor.hostInstanceId, lastPosition };
}

function applyHttpRouteMutation(
  list: network.HttpRouteList,
  mutation: network.HttpRouteListMutation,
): network.HttpRouteList {
  if (mutation.type === "Remove") {
    return { httpRoutes: list.httpRoutes.filter((route) => route.id !== mutation.content) };
  }
  const route: network.HttpRoute = {
    id: mutation.id,
    workspace: mutation.workspace,
    podId: mutation.podId,
    podPort: mutation.podPort,
    title: mutation.title,
    internal: mutation.internal,
    trustedTascarrelFrontend: mutation.trustedTascarrelFrontend,
    hostnamePrefix: mutation.hostnamePrefix,
  };
  return {
    httpRoutes: upsertById(list.httpRoutes, route).toSorted(compareTargets),
  };
}

function applyPortForwardMutation(
  list: network.PortForwardList,
  mutation: network.PortForwardListMutation,
): network.PortForwardList {
  if (mutation.type === "Remove") {
    return { portForwards: list.portForwards.filter((forward) => forward.id !== mutation.content) };
  }
  const forward: network.PortForward = {
    id: mutation.id,
    workspace: mutation.workspace,
    podId: mutation.podId,
    podPort: mutation.podPort,
    hostPort: mutation.hostPort,
    ...(mutation.title === undefined ? {} : { title: mutation.title }),
  };
  return {
    portForwards: upsertById(list.portForwards, forward).toSorted(compareTargets),
  };
}

function applyPodHostForwardMutation(
  list: network.PodHostForwardList,
  mutation: network.PodHostForwardListMutation,
): network.PodHostForwardList {
  if (mutation.type === "Remove") {
    return {
      podHostForwards: list.podHostForwards.filter((forward) => forward.id !== mutation.content),
    };
  }
  const forward: network.PodHostForward = {
    id: mutation.id,
    workspace: mutation.workspace,
    podId: mutation.podId,
    mapping: mutation.mapping,
    ...(mutation.title === undefined ? {} : { title: mutation.title }),
  };
  return {
    podHostForwards: upsertById(list.podHostForwards, forward).toSorted(comparePodHostTargets),
  };
}

function upsertById<T extends { id: string }>(items: readonly T[], value: T): T[] {
  const index = items.findIndex((item) => item.id === value.id);
  return index < 0
    ? [...items, value]
    : items.map((item, itemIndex) => itemIndex === index ? value : item);
}

function compareTargets(
  left: { podId: string; podPort: number },
  right: { podId: string; podPort: number },
): number {
  return left.podId.localeCompare(right.podId)
    || left.podPort - right.podPort;
}

function comparePodHostTargets(
  left: { podId: string; mapping: string },
  right: { podId: string; mapping: string },
): number {
  return left.podId.localeCompare(right.podId)
    || left.mapping.localeCompare(right.mapping, undefined, { numeric: true });
}
