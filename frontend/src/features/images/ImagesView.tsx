import { Box, Clock3, FileText, Images as ImagesIcon, LoaderCircle, RefreshCw } from "lucide-react";
import { useEffect, useState } from "react";

import { guestApi } from "../../api/client.ts";
import type { images, workspaces } from "../../api/generated/index.ts";
import { Badge } from "../../components/ui/Badge.tsx";
import { Button } from "../../components/ui/Button.tsx";
import { useImageLog, useImages } from "./state.ts";

export function ImagesView({ workspace }: { workspace: workspaces.WorkspaceName }) {
  const imageState = useImages(workspace);
  const inventory = imageState.value?.images ?? [];
  const [selectedId, setSelectedId] = useState<images.ImageId>();
  const [building, setBuilding] = useState(false);
  const [actionError, setActionError] = useState<string>();

  useEffect(() => setSelectedId(undefined), [workspace]);

  useEffect(() => {
    if (selectedId) return;
    setSelectedId(inventory.at(-1)?.id);
  }, [inventory, selectedId]);

  const selectedImage = inventory.find((image) => image.id === selectedId);
  const generating = inventory.some((image) => image.state.status === "Generating");
  const buildImage = async () => {
    if (building || generating) return;
    setBuilding(true);
    setActionError(undefined);
    try {
      const output = await guestApi(workspace).execute("images_Build", {});
      setSelectedId(output.imageId);
    } catch (cause) {
      setActionError(errorMessage(cause));
    } finally {
      setBuilding(false);
    }
  };

  return (
    <div className="flex h-full min-h-0 flex-col overflow-hidden bg-canvas text-foreground">
      <header className="flex flex-wrap items-center justify-between gap-3 border-b border-ui-border px-5 py-4">
        <div>
          <h1 className="flex items-center gap-2 text-sm font-semibold">
            <ImagesIcon aria-hidden="true" className="size-4 text-accent-text" /> Workspace Images
          </h1>
          <p className="mt-1 text-xs text-subtle">
            Build and inspect the execution images available to pods in {workspace}.
          </p>
        </div>
        <div className="flex items-center gap-2">
          <Button
            size="small"
            variant="primary"
            disabled={building || generating}
            onClick={() => void buildImage()}
          >
            {building || generating ? (
              <LoaderCircle aria-hidden="true" className="size-3.5 animate-spin" />
            ) : (
              <Box aria-hidden="true" className="size-3.5" />
            )}
            {generating ? "Build in progress" : building ? "Starting build…" : "Build image"}
          </Button>
        </div>
      </header>

      {actionError || imageState.error ? (
        <p className="border-b border-red-500/20 bg-red-500/5 px-5 py-2.5 text-xs text-red-200" role="alert">
          {actionError ?? imageState.error?.message}
        </p>
      ) : null}

      <div className="grid min-h-0 flex-1 grid-cols-1 overflow-hidden md:grid-cols-[19rem_minmax(0,1fr)]">
        <section className="min-h-0 overflow-y-auto border-b border-ui-border p-3 md:border-r md:border-b-0" aria-label="Image inventory">
          <div className="mb-2 flex items-center justify-between px-1 text-[11px] font-medium text-muted">
            <span>Images</span>
            <span className="font-mono text-subtle">{inventory.length}</span>
          </div>
          {inventory.toReversed().map((image) => (
            <button
              className="mb-1 flex w-full min-w-0 items-start gap-3 rounded-xl border border-transparent px-3 py-2.5 text-left text-xs transition hover:border-ui-border hover:bg-surface data-[selected=true]:border-ui-border-strong data-[selected=true]:bg-surface-raised"
              type="button"
              data-selected={image.id === selectedId}
              aria-pressed={image.id === selectedId}
              key={image.id}
              onClick={() => setSelectedId(image.id)}
            >
              <span className="min-w-0 flex-1">
                <span className="block truncate font-mono text-[11px] text-foreground">{shortId(image.id)}</span>
                <span className="mt-1 block text-[10px] text-subtle">{formatTimestamp(image.createdAt)}</span>
              </span>
              <ImageStateBadge state={image.state} />
            </button>
          ))}
          {inventory.length === 0 ? (
            <div className="rounded-xl border border-dashed border-ui-border p-5 text-center text-xs leading-5 text-subtle">
              {imageState.ready ? "No images have been built for this workspace." : "Loading image inventory…"}
            </div>
          ) : null}
        </section>

        {selectedImage ? (
          <ImageDetails image={selectedImage} workspace={workspace} />
        ) : (
          <div className="flex min-h-0 items-center justify-center p-8 text-center text-xs text-subtle">
            Select an image to inspect its build details and log.
          </div>
        )}
      </div>
    </div>
  );
}

function ImageDetails({
  image,
  workspace,
}: {
  image: images.Image;
  workspace: workspaces.WorkspaceName;
}) {
  const logState = useImageLog(workspace, image.id);
  const [updatingSeed, setUpdatingSeed] = useState(false);
  const [seedMessage, setSeedMessage] = useState<string>();
  const [seedError, setSeedError] = useState<string>();
  useEffect(() => {
    setUpdatingSeed(false);
    setSeedMessage(undefined);
    setSeedError(undefined);
  }, [image.id, workspace]);
  const updateSeed = async () => {
    if (updatingSeed || image.state.status !== "Available") return;
    setUpdatingSeed(true);
    setSeedMessage(undefined);
    setSeedError(undefined);
    try {
      const output = await guestApi(workspace).execute("images_UpdateWorkspaceSeed", {
        imageId: image.id,
      });
      setSeedMessage(output.updated ? "Workspace seed updated." : "Workspace seed is already current.");
    } catch (cause) {
      setSeedError(errorMessage(cause));
    } finally {
      setUpdatingSeed(false);
    }
  };
  return (
    <section className="flex min-h-0 flex-col overflow-hidden" aria-labelledby="image-detail-title">
      <div className="border-b border-ui-border px-5 py-4">
        <div className="flex flex-wrap items-center justify-between gap-3">
          <div className="min-w-0">
            <h2 className="truncate font-mono text-xs font-semibold text-foreground" id="image-detail-title">
              {image.id}
            </h2>
            <p className="mt-1 flex items-center gap-1.5 text-[11px] text-subtle">
              <Clock3 aria-hidden="true" className="size-3" /> Created {formatTimestamp(image.createdAt)}
            </p>
          </div>
          <div className="flex items-center gap-2">
            <ImageStateBadge state={image.state} />
            <Button
              size="small"
              disabled={updatingSeed || image.state.status !== "Available"}
              onClick={() => void updateSeed()}
            >
              {updatingSeed ? (
                <LoaderCircle aria-hidden="true" className="size-3.5 animate-spin" />
              ) : (
                <RefreshCw aria-hidden="true" className="size-3.5" />
              )}
              {updatingSeed ? "Updating seed…" : "Update workspace seed"}
            </Button>
          </div>
        </div>
        <dl className="mt-4 grid gap-3 text-[11px] sm:grid-cols-2">
          <div>
            <dt className="text-subtle">Input digest</dt>
            <dd className="mt-1 break-all font-mono text-muted">sha256:{image.input.sha256}</dd>
          </div>
          <div>
            <dt className="text-subtle">Input modified</dt>
            <dd className="mt-1 text-muted">{formatTimestamp(image.input.modifiedAt)}</dd>
          </div>
        </dl>
        {image.state.status === "Failed" ? (
          <p className="mt-4 rounded-lg border border-red-500/20 bg-red-500/5 px-3 py-2 text-xs leading-5 text-red-200" role="alert">
            {image.state.message}
          </p>
        ) : null}
        {seedError ? (
          <p className="mt-4 rounded-lg border border-red-500/20 bg-red-500/5 px-3 py-2 text-xs text-red-200" role="alert">
            {seedError}
          </p>
        ) : seedMessage ? (
          <p className="mt-4 text-[11px] text-subtle" role="status">{seedMessage}</p>
        ) : null}
      </div>
      <div className="flex min-h-0 flex-1 flex-col">
        <div className="flex items-center justify-between border-b border-ui-border px-5 py-2.5">
          <h3 className="flex items-center gap-2 text-xs font-medium text-muted">
            <FileText aria-hidden="true" className="size-3.5" /> Build Log
          </h3>
          <span className="text-[10px] text-subtle">{connectionLabel(logState.connection)}</span>
        </div>
        {logState.error ? (
          <p className="border-b border-red-500/20 px-5 py-2 text-xs text-red-200" role="alert">
            {logState.error.message}
          </p>
        ) : null}
        <div className="min-h-0 flex-1 overflow-auto bg-canvas p-4" role="log" aria-label={`Build log for ${image.id}`}>
          {logState.value?.length ? (
            <ol className="m-0 min-w-max list-none space-y-0.5 p-0 font-mono text-[11px] leading-5 text-muted">
              {logState.value.map((line) => (
                <li className="grid grid-cols-[3.5rem_4rem_minmax(0,1fr)] gap-3" key={String(line.line)}>
                  <span className="select-none text-right text-subtle">{String(line.line)}</span>
                  <span className="text-accent-text/70">{logSource(line.source)}</span>
                  <span className="whitespace-pre-wrap break-words text-muted">
                    {line.content}{line.truncated ? " …" : ""}
                  </span>
                </li>
              ))}
            </ol>
          ) : (
            <p className="m-0 text-xs text-subtle">
              {logState.ready ? "No retained build log is available." : "Loading build log…"}
            </p>
          )}
        </div>
      </div>
    </section>
  );
}

function ImageStateBadge({ state }: { state: images.ImageState }) {
  const tone = state.status === "Failed"
    ? "danger"
    : state.status === "Generating"
      ? "primary"
      : state.status === "Available"
        ? "success"
        : state.status === "Orphaned"
          ? "warning"
          : "muted";
  return <Badge size="xs" tone={tone}>{state.status}</Badge>;
}

function logSource(source: images.ImageLogSource): string {
  return source.type === "BuildKit" ? "buildkit" : "setup";
}

function connectionLabel(connection: "idle" | "connecting" | "live" | "reconnecting") {
  if (connection === "live") return "Live";
  if (connection === "idle") return "Paused";
  if (connection === "connecting") return "Connecting…";
  return "Reconnecting…";
}

function shortId(imageId: images.ImageId): string {
  const value = String(imageId);
  return value.length > 22 ? `${value.slice(0, 10)}…${value.slice(-8)}` : value;
}

function formatTimestamp(value: string): string {
  const date = new Date(value);
  return Number.isNaN(date.getTime()) ? value : date.toLocaleString();
}

function errorMessage(cause: unknown): string {
  return cause instanceof Error ? cause.message : String(cause);
}
