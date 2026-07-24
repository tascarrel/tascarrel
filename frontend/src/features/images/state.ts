import { guestApi } from "../../api/client.ts";
import type { images, store, workspaces } from "../../api/generated/index.ts";
import type { BackendStateDefinition } from "../../shared/state/BackendStateCache.ts";
import { useBackendState } from "../../shared/state/StateCacheProvider.tsx";
import { applyStoreEvent } from "../../shared/state/storeEvents.ts";

export function useImages(workspace: workspaces.WorkspaceName) {
  return useBackendState(imageListDefinition(workspace));
}

export function useImageLog(
  workspace: workspaces.WorkspaceName,
  imageId: images.ImageId,
) {
  return useBackendState(imageLogDefinition(workspace, imageId));
}

function imageListDefinition(
  workspace: workspaces.WorkspaceName,
): BackendStateDefinition<images.ImageList, images.ImageListChangedEvent, store.Stamp> {
  return {
    key: `guest/${workspace}/images`,
    connect: (cursor, handlers) => guestApi(workspace).subscribe(
      "images_Changed",
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
      (list, image) => {
        const index = list.images.findIndex((candidate) => candidate.id === image.id);
        const next = index < 0
          ? [...list.images, image]
          : list.images.map((candidate, candidateIndex) =>
              candidateIndex === index ? image : candidate
            );
        next.sort(compareImages);
        return { images: next };
      },
    ),
  };
}

function imageLogDefinition(
  workspace: workspaces.WorkspaceName,
  imageId: images.ImageId,
): BackendStateDefinition<readonly images.ImageLogLine[], images.ImageLogEvent, images.ImageLogLine["line"]> {
  return {
    key: `guest/${workspace}/image-log/${imageId}`,
    retention: "lru",
    connect: (cursor, handlers) => guestApi(workspace).subscribe(
      "images_Log",
      () => {
        const lastLine = cursor();
        return { imageId, ...(lastLine === undefined ? {} : { lastLine }) };
      },
      {
        onEvent: handlers.onEvent,
        onState: handlers.onConnection,
        onError: handlers.onError,
      },
    ),
    applyEvent: (current, event) => ({
      value: mergeLogLines(current ?? [], event.lines),
      cursor: event.lines.at(-1)?.line ?? current?.at(-1)?.line,
    }),
  };
}

function mergeLogLines(
  current: readonly images.ImageLogLine[],
  incoming: readonly images.ImageLogLine[],
): readonly images.ImageLogLine[] {
  const byLine = new Map(current.map((line) => [String(line.line), line]));
  for (const line of incoming) byLine.set(String(line.line), line);
  return [...byLine.values()].sort((left, right) =>
    String(left.line).localeCompare(String(right.line), undefined, { numeric: true })
  );
}

function compareImages(left: images.Image, right: images.Image): number {
  return String(left.createdAt).localeCompare(String(right.createdAt))
    || String(left.id).localeCompare(String(right.id));
}
