import { useEffect, useState } from "react";

import { guestApi } from "../../api/client.ts";
import type { guest, workspaces } from "../../api/generated/index.ts";
import type { BackendStateDefinition } from "../../shared/state/BackendStateCache.ts";
import { useBackendState } from "../../shared/state/StateCacheProvider.tsx";

const INFORMATION_CACHE = new Map<string, guest.QueryGuestInformationOutput>();
const INFORMATION_REQUESTS = new Map<string, Promise<guest.QueryGuestInformationOutput>>();
export const GUEST_METRIC_HISTORY_WINDOW_MS = 5 * 60 * 1_000;

export function useGuestMetrics(workspace: workspaces.WorkspaceName) {
  return useBackendState(guestMetricsDefinition(workspace));
}

export function useGuestInformation(
  workspace: workspaces.WorkspaceName,
  guestInstanceId: guest.GuestInstanceId | undefined,
) {
  const key = guestInstanceId === undefined ? undefined : `${workspace}/${guestInstanceId}`;
  const [retryRevision, setRetryRevision] = useState(0);
  const [snapshot, setSnapshot] = useState<{
    key?: string;
    value?: guest.QueryGuestInformationOutput;
    ready: boolean;
    error?: Error;
  }>(() => ({
    key,
    value: key === undefined ? undefined : INFORMATION_CACHE.get(key),
    ready: key !== undefined && INFORMATION_CACHE.has(key),
  }));

  useEffect(() => {
    if (key === undefined) {
      setSnapshot({ key, ready: false });
      return;
    }
    const cached = INFORMATION_CACHE.get(key);
    if (cached) {
      setSnapshot({ key, value: cached, ready: true });
      return;
    }
    let active = true;
    let retryHandle: number | undefined;
    setSnapshot({ key, ready: false });
    const request = INFORMATION_REQUESTS.get(key) ?? guestApi(workspace).execute("guest_QueryInformation", {});
    INFORMATION_REQUESTS.set(key, request);
    void request.then((value) => {
      INFORMATION_CACHE.set(key, value);
      if (active) setSnapshot({ key, value, ready: true });
    }).catch((cause: unknown) => {
      if (active) {
        setSnapshot({
          key,
          ready: true,
          error: cause instanceof Error ? cause : new Error(String(cause)),
        });
        retryHandle = window.setTimeout(() => setRetryRevision((current) => current + 1), 2_000);
      }
    }).finally(() => {
      if (INFORMATION_REQUESTS.get(key) === request) INFORMATION_REQUESTS.delete(key);
    });
    return () => {
      active = false;
      if (retryHandle !== undefined) window.clearTimeout(retryHandle);
    };
  }, [key, retryRevision, workspace]);

  return snapshot.key === key ? snapshot : { key, ready: false };
}

function guestMetricsDefinition(
  workspace: workspaces.WorkspaceName,
): BackendStateDefinition<readonly guest.GuestMetricsSample[], guest.GuestMetricsEvent, guest.GuestMetricsCursor> {
  return {
    key: `guest/${workspace}/metrics`,
    connect: (cursor, handlers) => guestApi(workspace).subscribe(
      "guest_Metrics",
      () => {
        const current = cursor();
        return current === undefined ? {} : { cursor: current };
      },
      {
        onEvent: handlers.onEvent,
        onState: handlers.onConnection,
        onError: handlers.onError,
      },
    ),
    applyEvent: (current, event) => {
      const merged = mergeSamples(current ?? [], event.samples);
      const latest = merged.at(-1);
      const latestTimestamp = latest ? Date.parse(String(latest.observedAt)) : undefined;
      const samples = latest && latestTimestamp !== undefined
        ? merged.filter((sample) =>
            sample.cursor.guestInstanceId === latest.cursor.guestInstanceId
              && Date.parse(String(sample.observedAt)) >= latestTimestamp - GUEST_METRIC_HISTORY_WINDOW_MS
          )
        : [];
      return { value: samples, cursor: samples.at(-1)?.cursor };
    },
  };
}

function mergeSamples(
  current: readonly guest.GuestMetricsSample[],
  incoming: readonly guest.GuestMetricsSample[],
): guest.GuestMetricsSample[] {
  const samples = new Map(current.map((sample) => [sampleKey(sample), sample]));
  for (const sample of incoming) samples.set(sampleKey(sample), sample);
  return [...samples.values()].sort((left, right) =>
    String(left.observedAt).localeCompare(String(right.observedAt))
      || String(left.cursor.position).localeCompare(String(right.cursor.position), undefined, { numeric: true })
  );
}

function sampleKey(sample: guest.GuestMetricsSample): string {
  return `${sample.cursor.guestInstanceId}/${sample.cursor.position}`;
}
