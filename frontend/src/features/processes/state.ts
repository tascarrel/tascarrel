import { useCallback } from "react";

import { guestApi } from "../../api/client.ts";
import type { processes, store, workspaces } from "../../api/generated/index.ts";
import type { BackendStateDefinition } from "../../shared/state/BackendStateCache.ts";
import { useBackendState } from "../../shared/state/StateCacheProvider.tsx";
import { applyStoreEvent } from "../../shared/state/storeEvents.ts";

export type ProcessTerminalChunk = Readonly<{
  startOffset: bigint;
  endOffset: bigint;
  data: Uint8Array;
}>;

export type ProcessTerminalReplica = Readonly<{
  chunks: readonly ProcessTerminalChunk[];
  nextOffset: bigint;
  retainedBytes: number;
  replaying: boolean;
  hasGap: boolean;
  checkpoint?: ProcessTerminalCheckpoint;
}>;

export type ProcessTerminalCheckpoint = Readonly<{
  offset: bigint;
  serialized: string;
  cols: number;
  rows: number;
}>;

export function useProcesses(workspace: workspaces.WorkspaceName) {
  return useBackendState(processListDefinition(workspace));
}

export function useProcessLog(
  workspace: workspaces.WorkspaceName,
  processId: processes.ProcessId,
) {
  return useBackendState(processLogDefinition(workspace, processId));
}

export function useProcessTerminal(
  workspace: workspaces.WorkspaceName,
  processId: processes.ProcessId,
) {
  const { updateValue, ...state } = useBackendState(
    processTerminalDefinition(workspace, processId),
  );
  const cacheCheckpoint = useCallback(
    (checkpoint: ProcessTerminalCheckpoint) => updateValue((current) =>
      cacheProcessTerminalCheckpoint(current, checkpoint)
    ),
    [updateValue],
  );
  return { ...state, cacheCheckpoint };
}

function processListDefinition(
  workspace: workspaces.WorkspaceName,
): BackendStateDefinition<processes.ProcessList, processes.ProcessListChangedEvent, store.Stamp> {
  return {
    key: `guest/${workspace}/processes`,
    connect: (cursor, handlers) => guestApi(workspace).subscribe(
      "processes_Changed",
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
          return { processes: list.processes.filter((process) => process.id !== mutation.content) };
        }
        const index = list.processes.findIndex((process) => process.id === mutation.id);
        const next = index < 0
          ? [...list.processes, mutation]
          : list.processes.map((process, candidateIndex) =>
              candidateIndex === index ? mutation : process
            );
        next.sort(compareProcesses);
        return { processes: next };
      },
    ),
  };
}

function processLogDefinition(
  workspace: workspaces.WorkspaceName,
  processId: processes.ProcessId,
): BackendStateDefinition<readonly processes.ProcessLogLine[], processes.ProcessLogEvent, processes.ProcessLogLine["line"]> {
  return {
    key: `guest/${workspace}/process-log/${processId}`,
    retention: "lru",
    connect: (cursor, handlers) => guestApi(workspace).subscribe(
      "processes_Log",
      () => {
        const lastLine = cursor();
        return { processId, ...(lastLine === undefined ? {} : { lastLine }) };
      },
      {
        onEvent: handlers.onEvent,
        onState: handlers.onConnection,
        onError: handlers.onError,
      },
    ),
    applyEvent: (current, event) => ({
      value: mergeLines(current ?? [], event.lines),
      cursor: event.lines.at(-1)?.line ?? current?.at(-1)?.line,
    }),
  };
}

function processTerminalDefinition(
  workspace: workspaces.WorkspaceName,
  processId: processes.ProcessId,
): BackendStateDefinition<ProcessTerminalReplica, ProcessTerminalCacheEvent, ProcessTerminalOffset> {
  return {
    key: `guest/${workspace}/process-terminal/${processId}`,
    retention: "lru",
    connect: (cursor, handlers) => guestApi(workspace).subscribe(
      "processes_Terminal",
      () => {
        const offset = cursor();
        return { processId, ...(offset === undefined ? {} : { offset }) };
      },
      {
        onEvent: (event) => handlers.onEvent({ type: "Backend", event }),
        onState: (state, attempt) => {
          if (state !== "live") handlers.onEvent({ type: "ReplayStarted" });
          handlers.onConnection(state, attempt);
        },
        onError: handlers.onError,
      },
      { eventCreditWindow: 1 },
    ),
    applyEvent: applyProcessTerminalEvent,
  };
}

function applyProcessTerminalEvent(
  current: ProcessTerminalReplica | undefined,
  event: ProcessTerminalCacheEvent,
): { value: ProcessTerminalReplica; cursor?: ProcessTerminalOffset } {
  const replica = current ?? EMPTY_TERMINAL_REPLICA;
  if (event.type === "ReplayStarted") {
    return {
      value: { ...replica, replaying: true },
      cursor: terminalCursor(replica.nextOffset),
    };
  }

  const update = event.event.update;
  if (update.type === "CaughtUp") {
    const boundary = terminalOffset(update.offset);
    if (boundary < replica.nextOffset) {
      throw new Error("The terminal subscription catch-up boundary moved backwards");
    }
    if (boundary > replica.nextOffset) {
      return {
        value: {
          chunks: [],
          nextOffset: boundary,
          retainedBytes: 0,
          replaying: false,
          hasGap: true,
        },
        cursor: terminalCursor(boundary),
      };
    }
    return {
      value: { ...replica, replaying: false },
      cursor: terminalCursor(boundary),
    };
  }

  const startOffset = terminalOffset(update.startOffset);
  const endOffset = terminalOffset(update.endOffset);
  const data = decodeTerminalData(update.data);
  if (endOffset < startOffset || endOffset - startOffset !== BigInt(data.length)) {
    throw new Error("The terminal subscription returned an invalid byte range");
  }
  if (endOffset <= replica.nextOffset) {
    return { value: replica, cursor: terminalCursor(replica.nextOffset) };
  }

  const hasStreamGap = startOffset > replica.nextOffset;
  const nextOffset = hasStreamGap ? startOffset : replica.nextOffset;
  const overlap = nextOffset > startOffset ? Number(nextOffset - startOffset) : 0;
  const appended = data.subarray(overlap);
  const chunks = hasStreamGap ? [] : replica.chunks;
  const retainedBytes = hasStreamGap ? 0 : replica.retainedBytes;
  const trimmed = trimTerminalChunks(
    [...chunks, { startOffset: nextOffset, endOffset, data: appended }],
    retainedBytes + appended.length,
  );
  const checkpoint = hasStreamGap
    ? undefined
    : retainedTerminalCheckpoint(replica.checkpoint, trimmed.chunks, endOffset);
  return {
    value: {
      chunks: trimmed.chunks,
      nextOffset: endOffset,
      retainedBytes: trimmed.retainedBytes,
      replaying: replica.replaying,
      hasGap: replica.hasGap || hasStreamGap || trimmed.evicted,
      ...(checkpoint === undefined ? {} : { checkpoint }),
    },
    cursor: terminalCursor(endOffset),
  };
}

function cacheProcessTerminalCheckpoint(
  current: ProcessTerminalReplica | undefined,
  checkpoint: ProcessTerminalCheckpoint,
): ProcessTerminalReplica | undefined {
  if (!current || checkpoint.offset > current.nextOffset) return current;
  const rewind = incompleteUtf8SuffixLength(current.chunks, checkpoint.offset);
  const normalized = {
    ...checkpoint,
    offset: checkpoint.offset - BigInt(rewind),
  };
  const retained = retainedTerminalCheckpoint(
    normalized,
    current.chunks,
    current.nextOffset,
  );
  if (!retained) return current;
  return { ...current, checkpoint: retained };
}

function retainedTerminalCheckpoint(
  checkpoint: ProcessTerminalCheckpoint | undefined,
  chunks: readonly ProcessTerminalChunk[],
  nextOffset: bigint,
): ProcessTerminalCheckpoint | undefined {
  if (!checkpoint || checkpoint.offset > nextOffset) return undefined;
  const firstOffset = chunks.at(0)?.startOffset ?? nextOffset;
  return checkpoint.offset >= firstOffset ? checkpoint : undefined;
}

/**
 * Finds bytes buffered by xterm's incremental UTF-8 decoder but absent from a
 * serialized framebuffer, so restoration can replay them from a safe offset.
 */
function incompleteUtf8SuffixLength(
  chunks: readonly ProcessTerminalChunk[],
  offset: bigint,
): number {
  const reversedBytes: number[] = [];
  for (let chunkIndex = chunks.length - 1; chunkIndex >= 0; chunkIndex -= 1) {
    const chunk = chunks[chunkIndex];
    if (offset <= chunk.startOffset) continue;
    const chunkEnd = offset < chunk.endOffset ? offset : chunk.endOffset;
    const endIndex = Number(chunkEnd - chunk.startOffset);
    for (let index = endIndex - 1; index >= 0 && reversedBytes.length < 4; index -= 1) {
      reversedBytes.push(chunk.data[index]);
    }
    if (reversedBytes.length === 4) break;
  }

  let continuationBytes = 0;
  while (
    continuationBytes < reversedBytes.length
    && reversedBytes[continuationBytes] >= 0x80
    && reversedBytes[continuationBytes] <= 0xbf
  ) continuationBytes += 1;
  const leadingByte = reversedBytes[continuationBytes];
  const expectedContinuationBytes = leadingByte >= 0xc2 && leadingByte <= 0xdf
    ? 1
    : leadingByte >= 0xe0 && leadingByte <= 0xef
    ? 2
    : leadingByte >= 0xf0 && leadingByte <= 0xf4
    ? 3
    : 0;
  return expectedContinuationBytes > continuationBytes ? continuationBytes + 1 : 0;
}

function trimTerminalChunks(
  chunks: readonly ProcessTerminalChunk[],
  retainedBytes: number,
): { chunks: readonly ProcessTerminalChunk[]; retainedBytes: number; evicted: boolean } {
  let excess = retainedBytes - TERMINAL_CACHE_BYTES;
  if (excess <= 0) return { chunks, retainedBytes, evicted: false };

  const retained: ProcessTerminalChunk[] = [];
  for (const chunk of chunks) {
    if (excess >= chunk.data.length) {
      excess -= chunk.data.length;
      continue;
    }
    if (excess > 0) {
      retained.push({
        startOffset: chunk.startOffset + BigInt(excess),
        endOffset: chunk.endOffset,
        data: chunk.data.subarray(excess),
      });
      excess = 0;
      continue;
    }
    retained.push(chunk);
  }
  return { chunks: retained, retainedBytes: TERMINAL_CACHE_BYTES, evicted: true };
}

function terminalOffset(value: processes.ProcessTerminalOutput["startOffset"]): bigint {
  return BigInt(String(value));
}

function terminalCursor(offset: bigint): ProcessTerminalOffset {
  return offset.toString() as ProcessTerminalOffset;
}

function decodeTerminalData(data: processes.ProcessTerminalData): Uint8Array {
  const binary = atob(data as string);
  return Uint8Array.from(binary, (character) => character.charCodeAt(0));
}

function mergeLines(
  current: readonly processes.ProcessLogLine[],
  incoming: readonly processes.ProcessLogLine[],
): readonly processes.ProcessLogLine[] {
  const byLine = new Map(current.map((line) => [String(line.line), line]));
  for (const line of incoming) byLine.set(String(line.line), line);
  return [...byLine.values()].sort((left, right) =>
    String(left.line).localeCompare(String(right.line), undefined, { numeric: true })
  );
}

function compareProcesses(left: processes.Process, right: processes.Process): number {
  return String(left.createdAt).localeCompare(String(right.createdAt))
    || String(left.id).localeCompare(String(right.id));
}

type ProcessTerminalOffset = NonNullable<processes.ProcessTerminalSubscription["offset"]>;

type ProcessTerminalCacheEvent =
  | Readonly<{ type: "ReplayStarted" }>
  | Readonly<{ type: "Backend"; event: processes.ProcessTerminalEvent }>;

const EMPTY_TERMINAL_REPLICA: ProcessTerminalReplica = {
  chunks: [],
  nextOffset: 0n,
  retainedBytes: 0,
  replaying: true,
  hasGap: false,
};

const TERMINAL_CACHE_BYTES = 16 * 1024 * 1024;
