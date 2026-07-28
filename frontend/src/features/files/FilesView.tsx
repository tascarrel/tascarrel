import {
  ChevronDown,
  ChevronRight,
  Download,
  File as FileIcon,
  FileQuestion,
  Folder,
  Link,
  LoaderCircle,
  RefreshCw,
} from "lucide-react";
import { lazy, Suspense, useCallback, useEffect, useRef, useState } from "react";

import { guestApi } from "../../api/client.ts";
import { workspaceFileUrl } from "../../api/files.ts";
import type { files, pods, workspaces } from "../../api/generated/index.ts";
import { Button } from "../../components/ui/Button.tsx";
import { SegmentedControl } from "../../components/ui/SegmentedControl.tsx";

const SyntaxHighlightedFile = lazy(() =>
  import("../../components/ui/SyntaxHighlightedFile.tsx").then((module) => ({
    default: module.SyntaxHighlightedFile,
  })),
);

const MarkdownContent = lazy(() =>
  import("../chat/index.ts").then((module) => ({ default: module.MarkdownContent })),
);

const DEFAULT_PREVIEW_BYTES = 2 * 1024 * 1024;
const MARKDOWN_REPRESENTATIONS = [
  { value: "source", label: "Source" },
  { value: "rendered", label: "Rendered" },
] as const;

type MarkdownRepresentation = typeof MARKDOWN_REPRESENTATIONS[number]["value"];

type DirectoryLoad = {
  entries?: readonly files.FileEntry[];
  loading: boolean;
  error?: string;
};

export function FilesView({
  workspace,
  pod,
}: {
  workspace: workspaces.WorkspaceName;
  pod: pods.Pod;
}) {
  const [directories, setDirectories] = useState<Record<string, DirectoryLoad>>({});
  const [expanded, setExpanded] = useState<ReadonlySet<string>>(() => new Set([""]));
  const [selectedPath, setSelectedPath] = useState<string>();
  const [previewRevision, setPreviewRevision] = useState(0);
  const requests = useRef(new Map<string, AbortController>());

  const readDirectory = useCallback(async (path: string, force = false) => {
    if (!force && directories[path]?.entries) return;
    requests.current.get(path)?.abort();
    const controller = new AbortController();
    requests.current.set(path, controller);
    setDirectories((current) => ({
      ...current,
      [path]: { ...current[path], loading: true, error: undefined },
    }));
    try {
      const output = await guestApi(workspace).execute("files_ReadDirectory", {
        podId: pod.id,
        path: path as files.FilePath,
      }, controller.signal);
      setDirectories((current) => ({
        ...current,
        [path]: { entries: output.entries, loading: false },
      }));
    } catch (cause) {
      if (controller.signal.aborted) return;
      setDirectories((current) => ({
        ...current,
        [path]: { ...current[path], loading: false, error: errorMessage(cause) },
      }));
    } finally {
      if (requests.current.get(path) === controller) requests.current.delete(path);
    }
  }, [directories, pod.id, workspace]);

  useEffect(() => {
    setDirectories({});
    setExpanded(new Set([""]));
    setSelectedPath(undefined);
    setPreviewRevision(0);
    for (const request of requests.current.values()) request.abort();
    requests.current.clear();
  }, [pod.id, workspace]);

  useEffect(() => {
    if (!directories[""]?.entries && !directories[""]?.loading) {
      void readDirectory("");
    }
  }, [directories, readDirectory]);

  useEffect(() => () => {
    for (const request of requests.current.values()) request.abort();
  }, []);

  const toggleDirectory = (path: string) => {
    const opening = !expanded.has(path);
    setExpanded((current) => {
      const next = new Set(current);
      if (opening) next.add(path);
      else next.delete(path);
      return next;
    });
    if (opening) void readDirectory(path);
  };

  const refresh = () => {
    for (const request of requests.current.values()) request.abort();
    requests.current.clear();
    setDirectories({});
    setPreviewRevision((revision) => revision + 1);
    void readDirectory("", true);
  };
  const selectedSize = selectedPath ? fileSize(directories, selectedPath) : undefined;

  return (
    <div className="grid h-full min-h-0 grid-cols-[minmax(14rem,22rem)_minmax(0,1fr)] overflow-hidden bg-canvas text-foreground">
      <section className="flex min-h-0 flex-col border-r border-ui-border" aria-label="Workspace files">
        <header className="flex items-center justify-between gap-3 border-b border-ui-border px-3 py-2.5">
          <div className="min-w-0">
            <h1 className="truncate text-xs font-semibold">/workspace</h1>
            <p className="mt-0.5 truncate text-[10px] text-subtle">{pod.title}</p>
          </div>
          <Button aria-label="Refresh workspace files" size="icon" onClick={refresh}>
            <RefreshCw aria-hidden="true" className="size-3.5" />
          </Button>
        </header>
        <div className="min-h-0 flex-1 overflow-auto py-1" role="region" aria-label="Workspace file tree">
          <DirectoryChildren
            directory=""
            depth={0}
            directories={directories}
            expanded={expanded}
            selectedPath={selectedPath}
            onToggle={toggleDirectory}
            onSelect={setSelectedPath}
            onRetry={(path) => void readDirectory(path, true)}
          />
        </div>
      </section>

      {selectedPath ? (
        <FilePreview
          workspace={workspace}
          podId={pod.id}
          path={selectedPath as files.FilePath}
          revision={previewRevision}
          size={selectedSize}
        />
      ) : (
        <div className="flex min-h-0 items-center justify-center p-8 text-center text-xs text-subtle">
          Select a file to preview it. Directories are read only when expanded.
        </div>
      )}
    </div>
  );
}

function DirectoryChildren({
  directory,
  depth,
  directories,
  expanded,
  selectedPath,
  onToggle,
  onSelect,
  onRetry,
}: {
  directory: string;
  depth: number;
  directories: Readonly<Record<string, DirectoryLoad>>;
  expanded: ReadonlySet<string>;
  selectedPath?: string;
  onToggle: (path: string) => void;
  onSelect: (path: string) => void;
  onRetry: (path: string) => void;
}) {
  const load = directories[directory];
  if (load?.loading && !load.entries) {
    return (
      <p className="flex items-center gap-2 px-3 py-2 text-[11px] text-subtle">
        <LoaderCircle aria-hidden="true" className="size-3 animate-spin" /> Loading…
      </p>
    );
  }
  if (load?.error && !load.entries) {
    return (
      <button
        className="mx-2 my-1 rounded-lg border border-red-500/20 px-2 py-1.5 text-left text-[10px] text-red-200"
        type="button"
        onClick={() => onRetry(directory)}
      >
        {load.error} Click to retry.
      </button>
    );
  }
  if (!load?.entries?.length) {
    return <p className="px-3 py-2 text-[10px] text-subtle">Empty directory</p>;
  }

  return load.entries.map((entry) => {
    const path = joinPath(directory, entry.name);
    const isDirectory = entry.kind.tag === "Directory";
    const isExpanded = isDirectory && expanded.has(path);
    const EntryIcon = fileIcon(entry.kind);
    return (
      <div key={path}>
        <button
          aria-expanded={isDirectory ? isExpanded : undefined}
          aria-pressed={!isDirectory && entry.kind.tag === "File" ? selectedPath === path : undefined}
          className="group flex h-7 w-full min-w-0 items-center gap-1.5 pr-2 text-left text-[11px] text-muted outline-none hover:bg-surface hover:text-foreground focus-visible:bg-surface-raised focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-accent disabled:cursor-default disabled:opacity-60 data-[selected=true]:bg-accent/10 data-[selected=true]:text-accent-text"
          data-selected={!isDirectory && selectedPath === path}
          disabled={!isDirectory && entry.kind.tag !== "File"}
          style={{ paddingLeft: `${8 + depth * 14}px` }}
          type="button"
          onClick={() => isDirectory ? onToggle(path) : entry.kind.tag === "File" && onSelect(path)}
        >
          {isDirectory ? (
            isExpanded
              ? <ChevronDown aria-hidden="true" className="size-3 shrink-0" />
              : <ChevronRight aria-hidden="true" className="size-3 shrink-0" />
          ) : <span aria-hidden="true" className="w-3 shrink-0" />}
          <EntryIcon aria-hidden="true" className="size-3.5 shrink-0 text-subtle group-hover:text-muted" />
          <span className="min-w-0 flex-1 truncate">{entry.name}</span>
          {entry.size !== undefined ? (
            <span className="shrink-0 font-mono text-[9px] text-subtle">{formatBytes(entry.size)}</span>
          ) : null}
          {entry.gitStatus ? <GitStatus status={entry.gitStatus} /> : null}
        </button>
        {isExpanded ? (
          <div>
            <DirectoryChildren
              directory={path}
              depth={depth + 1}
              directories={directories}
              expanded={expanded}
              selectedPath={selectedPath}
              onToggle={onToggle}
              onSelect={onSelect}
              onRetry={onRetry}
            />
          </div>
        ) : null}
      </div>
    );
  });
}

function FilePreview({
  workspace,
  podId,
  path,
  revision,
  size,
}: {
  workspace: workspaces.WorkspaceName;
  podId: pods.PodId;
  path: files.FilePath;
  revision: number;
  size?: files.FileEntry["size"];
}) {
  const [preview, setPreview] = useState<Preview>({ status: "loading" });
  const [markdownRepresentation, setMarkdownRepresentation] =
    useState<MarkdownRepresentation>("source");
  const url = workspaceFileUrl(workspace, podId, path);
  const markdown = isMarkdownPath(String(path));

  useEffect(() => {
    const controller = new AbortController();
    setPreview({ status: "loading" });
    void loadPreview(url, String(path), controller.signal).then(setPreview, (cause) => {
      if (!controller.signal.aborted) setPreview({ status: "error", message: errorMessage(cause) });
    });
    return () => controller.abort();
  }, [path, revision, url]);

  return (
    <section className="flex min-h-0 flex-col overflow-hidden" aria-label={`Preview ${path}`}>
      <header className="flex items-center justify-between gap-3 border-b border-ui-border px-4 py-2.5">
        <div className="min-w-0">
          <h2 className="truncate font-mono text-xs font-medium">{path}</h2>
          {size !== undefined || (preview.status === "text" && preview.truncated) ? (
            <p className={`mt-0.5 text-[10px] ${preview.status === "text" && preview.truncated ? "text-amber-300" : "text-subtle"}`}>
              {size !== undefined ? formatBytes(size) : null}
              {size !== undefined && preview.status === "text" && preview.truncated ? " · " : null}
              {preview.status === "text" && preview.truncated ? "Preview truncated at 2 MiB" : null}
            </p>
          ) : null}
        </div>
        <div className="flex shrink-0 items-center gap-2">
          {markdown ? (
            <SegmentedControl
              label="Markdown representation"
              options={MARKDOWN_REPRESENTATIONS}
              value={markdownRepresentation}
              onValueChange={setMarkdownRepresentation}
            />
          ) : null}
          <a
            className="inline-flex h-8 shrink-0 items-center gap-2 rounded-lg border border-ui-border/70 bg-surface px-2.5 text-xs font-medium text-muted outline-none transition hover:border-ui-border-strong hover:bg-surface-raised hover:text-foreground focus-visible:outline-2 focus-visible:outline-accent"
            download={fileName(String(path))}
            href={workspaceFileUrl(workspace, podId, path, true)}
          >
            <Download aria-hidden="true" className="size-3.5" /> Download
          </a>
        </div>
      </header>
      <div className="min-h-0 flex-1 overflow-auto">
        {preview.status === "loading" ? (
          <p className="flex items-center gap-2 p-4 text-xs text-subtle">
            <LoaderCircle aria-hidden="true" className="size-3.5 animate-spin" /> Loading preview…
          </p>
        ) : preview.status === "error" ? (
          <p className="m-4 rounded-lg border border-red-500/20 bg-red-500/5 p-3 text-xs text-red-200" role="alert">
            {preview.message}
          </p>
        ) : preview.status === "image" ? (
          <div className="flex h-full min-h-0 items-center justify-center overflow-hidden p-6">
            <img className="max-h-full max-w-full object-contain" src={url} alt={String(path)} />
          </div>
        ) : preview.status === "binary" ? (
          <div className="flex min-h-full flex-col items-center justify-center gap-3 p-8 text-center text-xs text-subtle">
            <FileQuestion aria-hidden="true" className="size-8" />
            This file does not have a text preview. Download it to inspect its contents.
          </div>
        ) : markdown && markdownRepresentation === "rendered" ? (
          <Suspense fallback={<p className="p-4 text-xs text-subtle">Rendering Markdown…</p>}>
            <div className="min-h-full px-6 py-4">
              <MarkdownContent content={preview.text} workspacePath={String(path)} />
            </div>
          </Suspense>
        ) : (
          <Suspense fallback={<FilePreviewFallback text={preview.text} />}>
            <SyntaxHighlightedFile contents={preview.text} name={String(path)} />
          </Suspense>
        )}
      </div>
    </section>
  );
}

function FilePreviewFallback({ text }: { text: string }) {
  return <pre className="m-0 min-w-max p-4 font-mono text-[11px] leading-5 text-muted">{text}</pre>;
}

type Preview =
  | { status: "loading" }
  | { status: "error"; message: string }
  | { status: "image" }
  | { status: "binary" }
  | { status: "text"; text: string; truncated: boolean };

async function loadPreview(url: string, path: string, signal: AbortSignal): Promise<Preview> {
  const response = await fetch(url, { signal });
  if (!response.ok) throw new Error(await responseError(response));
  if (response.headers.get("content-type")?.startsWith("image/")) {
    await response.body?.cancel();
    return { status: "image" };
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
  if (/\.(?:pdf|zip|gz|xz|zst|tar|wasm|woff2?|ttf|exe|bin)$/i.test(path)) return true;
  const sample = bytes.subarray(0, Math.min(bytes.length, 8 * 1024));
  return sample.includes(0);
}

function isMarkdownPath(path: string): boolean {
  return /\.(?:md|markdown|mdown|mkd)$/i.test(path);
}

function fileIcon(kind: files.FileKind) {
  switch (kind.tag) {
    case "Directory": return Folder;
    case "File": return FileIcon;
    case "Symlink": return Link;
    case "Other": return FileQuestion;
  }
}

function GitStatus({ status }: { status: files.FileGitStatus }) {
  const change = status.worktree ?? status.index;
  if (!change) return null;
  const label = change.tag === "Untracked" ? "U" : change.tag.slice(0, 1);
  const tone = change.tag === "Deleted" || change.tag === "Unmerged"
    ? "text-red-300"
    : change.tag === "Added" || change.tag === "Untracked"
      ? "text-emerald-300"
      : "text-amber-300";
  return <span aria-label={change.tag} className={`shrink-0 font-mono text-[10px] ${tone}`} title={change.tag}>{label}</span>;
}

function joinPath(parent: string, child: string): string {
  return parent ? `${parent}/${child}` : child;
}

function fileSize(
  directories: Readonly<Record<string, DirectoryLoad>>,
  path: string,
): files.FileEntry["size"] | undefined {
  const separator = path.lastIndexOf("/");
  const parent = separator < 0 ? "" : path.slice(0, separator);
  const name = path.slice(separator + 1);
  return directories[parent]?.entries?.find((entry) => entry.name === name)?.size;
}

function fileName(path: string): string {
  return path.slice(path.lastIndexOf("/") + 1) || "file";
}

function formatBytes(value: string | number): string {
  const bytes = Number(value);
  if (bytes >= 1024 * 1024) return `${(bytes / (1024 * 1024)).toFixed(1)} MiB`;
  if (bytes >= 1024) return `${(bytes / 1024).toFixed(1)} KiB`;
  return `${String(bytes)} B`;
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
