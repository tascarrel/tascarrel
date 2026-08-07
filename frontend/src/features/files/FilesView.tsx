import {
  ChevronDown,
  ChevronRight,
  Download,
  File as FileIcon,
  FileQuestion,
  Folder,
  Link,
  LoaderCircle,
  Pencil,
  RefreshCw,
} from "lucide-react";
import { lazy, useCallback, useEffect, useRef, useState } from "react";

import { guestApi } from "../../api/client.ts";
import {
  fileRootKey,
  fileRootPath,
  podFilePath,
  podFileUrl,
  WORKSPACE_FILE_ROOT,
} from "../../api/files.ts";
import type { files, pods, workspaces } from "../../api/generated/index.ts";
import { Button } from "../../components/ui/Button.tsx";
import { ConfirmDialog } from "../../components/ui/ConfirmDialog.tsx";
import { SegmentedControl } from "../../components/ui/SegmentedControl.tsx";
import { SelectControl } from "../../components/ui/SelectControl.tsx";
import {
  isMarkdownPath,
  MARKDOWN_REPRESENTATIONS,
  type MarkdownRepresentation,
  type PodTextFile,
  PodFileViewer,
} from "./PodFileViewer.tsx";
import { PodFileEditor } from "./PodFileEditor.tsx";

const MarkdownContent = lazy(() =>
  import("../chat/index.ts").then((module) => ({ default: module.MarkdownContent })),
);

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
  const [roots, setRoots] = useState<readonly files.FileRoot[]>([WORKSPACE_FILE_ROOT]);
  const [selectedRoot, setSelectedRoot] = useState<files.FileRoot>(WORKSPACE_FILE_ROOT);
  const [rootsError, setRootsError] = useState<string>();
  const [directories, setDirectories] = useState<Record<string, DirectoryLoad>>({});
  const [expanded, setExpanded] = useState<ReadonlySet<string>>(() => new Set([""]));
  const [selectedPath, setSelectedPath] = useState<string>();
  const [previewRevision, setPreviewRevision] = useState(0);
  const [editorDirty, setEditorDirty] = useState(false);
  const [editorSaving, setEditorSaving] = useState(false);
  const [pendingNavigation, setPendingNavigation] = useState<PendingNavigation>();
  const requests = useRef(new Map<string, AbortController>());
  const selectedRootKey = fileRootKey(selectedRoot);

  useEffect(() => {
    const controller = new AbortController();
    setRoots([WORKSPACE_FILE_ROOT]);
    setSelectedRoot(WORKSPACE_FILE_ROOT);
    setRootsError(undefined);
    void guestApi(workspace).execute("files_ListRoots", { podId: pod.id }, controller.signal)
      .then((output) => setRoots(output.roots), (cause) => {
        if (!controller.signal.aborted) setRootsError(errorMessage(cause));
      });
    return () => controller.abort();
  }, [pod.id, workspace]);

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
        root: selectedRoot,
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
  }, [directories, pod.id, selectedRoot, workspace]);

  useEffect(() => {
    setDirectories({});
    setExpanded(new Set([""]));
    setSelectedPath(undefined);
    setPreviewRevision(0);
    for (const request of requests.current.values()) request.abort();
    requests.current.clear();
  }, [pod.id, selectedRootKey, workspace]);

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
  const navigate = (navigation: PendingNavigation) => {
    if (editorDirty || editorSaving) {
      setPendingNavigation(navigation);
      return;
    }
    applyNavigation(navigation);
  };
  const applyNavigation = (navigation: PendingNavigation) => {
    setEditorDirty(false);
    setEditorSaving(false);
    if (navigation.tag === "path") setSelectedPath(navigation.path);
    else {
      setSelectedPath(undefined);
      setSelectedRoot(navigation.root);
    }
  };
  const selectedSize = selectedPath ? fileSize(directories, selectedPath) : undefined;

  return (
    <div className="grid h-full min-h-0 grid-cols-[minmax(14rem,22rem)_minmax(0,1fr)] overflow-hidden bg-canvas text-foreground">
      <section className="flex min-h-0 flex-col border-r border-ui-border" aria-label="Pod files">
        <header className="flex items-center justify-between gap-3 border-b border-ui-border px-3 py-2.5">
          <div className="min-w-0 flex-1">
            <h1 className="sr-only">Pod files</h1>
            <SelectControl
              className="w-full"
              label="File root"
              options={roots.map((root) => ({
                label: fileRootPath(root),
                value: fileRootKey(root),
              }))}
              value={selectedRootKey}
              variant="sidebar"
              onChange={(value) => {
                const root = roots.find((candidate) => fileRootKey(candidate) === value);
                if (root && fileRootKey(root) !== selectedRootKey) navigate({ tag: "root", root });
              }}
            />
            {rootsError ? (
              <p className="mt-0.5 truncate text-[10px] text-red-300" role="alert">
                {rootsError}
              </p>
            ) : (
              <p className="mt-0.5 truncate text-[10px] text-subtle">{pod.title}</p>
            )}
          </div>
          <Button
            aria-label={`Refresh ${fileRootPath(selectedRoot)} files`}
            disabled={editorDirty || editorSaving}
            size="icon"
            title={editorDirty || editorSaving ? "Finish editing before refreshing" : "Refresh files"}
            onClick={refresh}
          >
            <RefreshCw aria-hidden="true" className="size-3.5" />
          </Button>
        </header>
        <div className="min-h-0 flex-1 overflow-auto py-1" role="region" aria-label={`${fileRootPath(selectedRoot)} file tree`}>
          <DirectoryChildren
            directory=""
            depth={0}
            directories={directories}
            expanded={expanded}
            selectedPath={selectedPath}
            onToggle={toggleDirectory}
            onSelect={(path) => {
              if (path !== selectedPath) navigate({ tag: "path", path });
            }}
            onRetry={(path) => void readDirectory(path, true)}
          />
        </div>
      </section>

      {selectedPath ? (
        <FilePreview
          key={`${selectedRootKey}:${selectedPath}`}
          workspace={workspace}
          podId={pod.id}
          root={selectedRoot}
          path={selectedPath as files.FilePath}
          revision={previewRevision}
          size={selectedSize}
          onDirtyChange={setEditorDirty}
          onSavingChange={setEditorSaving}
        />
      ) : (
        <div className="flex min-h-0 items-center justify-center p-8 text-center text-xs text-subtle">
          Select a file to preview it. Directories are read only when expanded.
        </div>
      )}
      <ConfirmDialog
        confirmLabel={editorDirty ? "Discard changes" : "Continue"}
        description={editorSaving
          ? "Wait for the current save to finish before navigating away."
          : editorDirty
            ? "Your unsaved file edits will be lost when you navigate away."
            : "The save finished. Continue to the selected file?"}
        open={pendingNavigation !== undefined}
        pending={editorSaving}
        title="Discard unsaved changes?"
        onConfirm={() => {
          if (pendingNavigation) applyNavigation(pendingNavigation);
          setPendingNavigation(undefined);
        }}
        onOpenChange={(open) => {
          if (!open) setPendingNavigation(undefined);
        }}
      />
    </div>
  );
}

type PendingNavigation =
  | { tag: "path"; path: string }
  | { tag: "root"; root: files.FileRoot };

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
  root,
  path,
  revision,
  size,
  onDirtyChange,
  onSavingChange,
}: {
  workspace: workspaces.WorkspaceName;
  podId: pods.PodId;
  root: files.FileRoot;
  path: files.FilePath;
  revision: number;
  size?: files.FileEntry["size"];
  onDirtyChange: (dirty: boolean) => void;
  onSavingChange: (saving: boolean) => void;
}) {
  const [markdownRepresentation, setMarkdownRepresentation] =
    useState<MarkdownRepresentation>("source");
  const [textFile, setTextFile] = useState<PodTextFile>();
  const [editing, setEditing] = useState(false);
  const [localRevision, setLocalRevision] = useState(0);
  const markdown = isMarkdownPath(String(path));
  const absolutePath = podFilePath(root, String(path));

  useEffect(() => {
    setTextFile(undefined);
    setEditing(false);
    setLocalRevision(0);
    onDirtyChange(false);
    onSavingChange(false);
  }, [onDirtyChange, onSavingChange, path, root]);

  useEffect(() => () => {
    onDirtyChange(false);
    onSavingChange(false);
  }, [onDirtyChange, onSavingChange]);

  return (
    <section className="flex min-h-0 flex-col overflow-hidden" aria-label={`Preview ${absolutePath}`}>
      <header className="flex items-center justify-between gap-3 border-b border-ui-border px-4 py-2.5">
        <div className="min-w-0">
          <h2 className="truncate font-mono text-xs font-medium">{absolutePath}</h2>
          {size !== undefined ? (
            <p className="mt-0.5 text-[10px] text-subtle">{formatBytes(size)}</p>
          ) : null}
        </div>
        <div className="flex shrink-0 items-center gap-2">
          {markdown && !editing ? (
            <SegmentedControl
              label="Markdown representation"
              options={MARKDOWN_REPRESENTATIONS}
              value={markdownRepresentation}
              onValueChange={setMarkdownRepresentation}
            />
          ) : null}
          {textFile && !textFile.writable ? (
            <span className="text-[10px] text-subtle">Read-only</span>
          ) : null}
          {textFile?.writable && !editing ? (
            <Button size="small" onClick={() => setEditing(true)}>
              <Pencil aria-hidden="true" className="size-3.5" /> Edit
            </Button>
          ) : null}
          <a
            className="inline-flex h-8 shrink-0 items-center gap-2 rounded-lg border border-ui-border/70 bg-surface px-2.5 text-xs font-medium text-muted outline-none transition hover:border-ui-border-strong hover:bg-surface-raised hover:text-foreground focus-visible:outline-2 focus-visible:outline-accent"
            download={fileName(String(path))}
            href={podFileUrl(workspace, podId, root, path, true)}
          >
            <Download aria-hidden="true" className="size-3.5" /> Download
          </a>
        </div>
      </header>
      <div className="min-h-0 flex-1 overflow-hidden">
        {editing && textFile ? (
          <PodFileEditor
            key={`${fileRootKey(root)}:${String(path)}`}
            initial={textFile}
            path={path}
            podId={podId}
            renderMarkdown={(content, markdownRoot, markdownPath) => (
              <MarkdownContent content={content} fileTarget={{ root: markdownRoot, path: markdownPath }} />
            )}
            root={root}
            workspace={workspace}
            onCancel={() => {
              setEditing(false);
              onDirtyChange(false);
              onSavingChange(false);
            }}
            onDirtyChange={onDirtyChange}
            onSavingChange={onSavingChange}
            onReload={() => {
              setEditing(false);
              setTextFile(undefined);
              setLocalRevision((current) => current + 1);
              onDirtyChange(false);
              onSavingChange(false);
            }}
            onSaved={(file) => {
              setTextFile(file);
              setLocalRevision((current) => current + 1);
            }}
          />
        ) : (
          <PodFileViewer
            markdownRepresentation={markdownRepresentation}
            path={path}
            podId={podId}
            renderMarkdown={(content, markdownRoot, markdownPath) => (
              <MarkdownContent content={content} fileTarget={{ root: markdownRoot, path: markdownPath }} />
            )}
            revision={revision + localRevision}
            root={root}
            workspace={workspace}
            onTextFile={setTextFile}
          />
        )}
      </div>
    </section>
  );
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

function errorMessage(cause: unknown): string {
  return cause instanceof Error ? cause.message : String(cause);
}
