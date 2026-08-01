import { FileQuestion, LoaderCircle } from "lucide-react";
import { lazy, Suspense, type ReactNode, useEffect, useState } from "react";

import { podFileUrl } from "../../api/files.ts";
import type { files, pods, workspaces } from "../../api/generated/index.ts";
import { PdfViewer } from "../../components/pdf/index.ts";

export type MarkdownRepresentation = "rendered" | "source";
export type PodMarkdownRenderer = (
  content: string,
  root: files.FileRoot,
  path: string,
) => ReactNode;

/** Loads and renders one file from a pod-visible file root. */
export function PodFileViewer({
  workspace,
  podId,
  root,
  path,
  markdownRepresentation,
  renderMarkdown,
  line,
  revision = 0,
}: {
  workspace: workspaces.WorkspaceName;
  podId: pods.PodId;
  root: files.FileRoot;
  path: files.FilePath;
  markdownRepresentation: MarkdownRepresentation;
  renderMarkdown: PodMarkdownRenderer;
  line?: number;
  revision?: number;
}) {
  const [preview, setPreview] = useState<Preview>({ status: "loading" });
  const [imageState, setImageState] = useState<ImageState>("loading");
  const url = podFileUrl(workspace, podId, root, path);
  const markdown = isMarkdownPath(String(path));

  useEffect(() => {
    const controller = new AbortController();
    setPreview({ status: "loading" });
    setImageState("loading");
    void loadPreview(url, String(path), controller.signal).then(setPreview, (cause) => {
      if (!controller.signal.aborted) {
        setPreview({ status: "error", message: errorMessage(cause) });
      }
    });
    return () => controller.abort();
  }, [path, revision, url]);

  if (preview.status === "loading") {
    return (
      <p className="flex items-center gap-2 p-4 text-xs text-subtle" role="status">
        <LoaderCircle aria-hidden="true" className="size-3.5 animate-spin" /> Loading preview…
      </p>
    );
  }
  if (preview.status === "error") {
    return <FilePreviewError message={preview.message} />;
  }
  if (preview.status === "image") {
    if (imageState === "error") {
      return <FilePreviewError message="The image could not be rendered." />;
    }
    return (
      <div className="relative flex h-full min-h-0 items-center justify-center overflow-auto bg-black/30 p-6">
        <img
          className={`max-h-full max-w-full object-contain transition-opacity ${
            imageState === "loading" ? "opacity-0" : "opacity-100"
          }`}
          src={url}
          alt={String(path)}
          onError={() => setImageState("error")}
          onLoad={() => setImageState("ready")}
        />
        {imageState === "loading" ? (
          <div className="absolute inset-0 grid place-items-center" role="status">
            <LoaderCircle aria-hidden="true" className="size-5 animate-spin text-muted" />
            <span className="sr-only">Loading image</span>
          </div>
        ) : null}
      </div>
    );
  }
  if (preview.status === "pdf") {
    return <PdfViewer className="h-full" source={url} />;
  }
  if (preview.status === "binary") {
    return (
      <div className="flex min-h-full flex-col items-center justify-center gap-3 p-8 text-center text-xs text-subtle">
        <FileQuestion aria-hidden="true" className="size-8" />
        This file does not have a preview. Download it to inspect its contents.
      </div>
    );
  }

  return (
    <div className="flex h-full min-h-0 flex-col">
      {preview.truncated ? (
        <p className="shrink-0 border-b border-amber-500/20 bg-amber-500/5 px-4 py-2 text-[10px] text-amber-200">
          Preview truncated at 2 MiB.
        </p>
      ) : null}
      <div className="min-h-0 flex-1 overflow-auto">
        {markdown && markdownRepresentation === "rendered" ? (
          <Suspense fallback={<p className="p-4 text-xs text-subtle">Rendering Markdown…</p>}>
            <div className="min-h-full px-6 py-4">
              {renderMarkdown(preview.text, root, String(path))}
            </div>
          </Suspense>
        ) : (
          <Suspense fallback={<FilePreviewFallback text={preview.text} />}>
            <SyntaxHighlightedFile contents={preview.text} line={line} name={String(path)} />
          </Suspense>
        )}
      </div>
    </div>
  );
}

export const MARKDOWN_REPRESENTATIONS = [
  { value: "rendered", label: "Rendered" },
  { value: "source", label: "Source" },
] as const satisfies ReadonlyArray<{ value: MarkdownRepresentation; label: string }>;

/** Returns whether a pod file path should offer Markdown source and rendered views. */
export function isMarkdownPath(path: string): boolean {
  return /\.(?:md|markdown|mdown|mkd)$/i.test(path);
}

const SyntaxHighlightedFile = lazy(() =>
  import("../../components/ui/SyntaxHighlightedFile.tsx").then((module) => ({
    default: module.SyntaxHighlightedFile,
  })),
);

const DEFAULT_PREVIEW_BYTES = 2 * 1024 * 1024;

function FilePreviewError({ message }: { message: string }) {
  return (
    <p
      className="m-4 rounded-lg border border-red-500/20 bg-red-500/5 p-3 text-xs text-red-200"
      role="alert"
    >
      {message}
    </p>
  );
}

function FilePreviewFallback({ text }: { text: string }) {
  return <pre className="m-0 min-w-max p-4 font-mono text-[11px] leading-5 text-muted">{text}</pre>;
}

type Preview =
  | { status: "loading" }
  | { status: "error"; message: string }
  | { status: "image" }
  | { status: "pdf" }
  | { status: "binary" }
  | { status: "text"; text: string; truncated: boolean };

type ImageState = "loading" | "ready" | "error";

async function loadPreview(url: string, path: string, signal: AbortSignal): Promise<Preview> {
  const response = await fetch(url, { signal });
  if (!response.ok) throw new Error(await responseError(response));
  const contentType = response.headers.get("content-type")?.split(";", 1)[0].toLowerCase();
  if (contentType?.startsWith("image/")) {
    await response.body?.cancel();
    return { status: "image" };
  }
  if (contentType === "application/pdf" || /\.pdf$/i.test(path)) {
    await response.body?.cancel();
    return { status: "pdf" };
  }

  const reader = response.body?.getReader();
  if (!reader) return { status: "text", text: "", truncated: false };
  const chunks: Uint8Array[] = [];
  let length = 0;
  let truncated = false;
  while (length <= DEFAULT_PREVIEW_BYTES) {
    const result = await reader.read();
    if (result.done) break;
    chunks.push(result.value);
    length += result.value.byteLength;
    if (length > DEFAULT_PREVIEW_BYTES) {
      truncated = true;
      await reader.cancel();
      break;
    }
  }

  const bytes = new Uint8Array(Math.min(length, DEFAULT_PREVIEW_BYTES));
  let offset = 0;
  for (const chunk of chunks) {
    const available = Math.min(chunk.byteLength, bytes.byteLength - offset);
    bytes.set(chunk.subarray(0, available), offset);
    offset += available;
    if (offset === bytes.byteLength) break;
  }
  if (looksBinary(bytes, path)) return { status: "binary" };
  return { status: "text", text: new TextDecoder().decode(bytes), truncated };
}

function looksBinary(bytes: Uint8Array, path: string): boolean {
  if (/\.(?:zip|gz|xz|zst|tar|wasm|woff2?|ttf|exe|bin)$/i.test(path)) return true;
  const sample = bytes.subarray(0, Math.min(bytes.length, 8 * 1024));
  return sample.includes(0);
}

async function responseError(response: Response): Promise<string> {
  const body = (await response.text()).trim();
  try {
    const parsed = JSON.parse(body) as { message?: unknown };
    if (typeof parsed.message === "string") return parsed.message;
  } catch {
    // Plain-text gateway diagnostics are suitable for display.
  }
  return body || `File read failed with status ${response.status}`;
}

function errorMessage(cause: unknown): string {
  return cause instanceof Error ? cause.message : String(cause);
}
