import { AlertTriangle, LoaderCircle, RotateCcw, Save } from "lucide-react";
import { lazy, Suspense, useCallback, useEffect, useMemo, useState } from "react";

import {
  POD_TEXT_FILE_BYTE_LIMIT,
  PodFileConflictError,
  savePodTextFile,
} from "../../api/files.ts";
import type { files, pods, workspaces } from "../../api/generated/index.ts";
import { Button } from "../../components/ui/Button.tsx";
import { ConfirmDialog } from "../../components/ui/ConfirmDialog.tsx";
import { SegmentedControl } from "../../components/ui/SegmentedControl.tsx";
import {
  isMarkdownPath,
  MARKDOWN_REPRESENTATIONS,
  type MarkdownRepresentation,
  type PodMarkdownRenderer,
  type PodTextFile,
} from "./PodFileViewer.tsx";

const LightweightCodeEditor = lazy(() => import("./LightweightCodeEditor.tsx"));

/** Edits and revision-safely saves one writable pod text file. */
export function PodFileEditor({
  workspace,
  podId,
  root,
  path,
  initial,
  line,
  renderMarkdown,
  confirmLayer = "default",
  onCancel,
  onReload,
  onSaved,
  onDirtyChange,
  onSavingChange,
}: {
  workspace: workspaces.WorkspaceName;
  podId: pods.PodId;
  root: files.FileRoot;
  path: files.FilePath;
  initial: PodTextFile;
  line?: number;
  renderMarkdown: PodMarkdownRenderer;
  confirmLayer?: "default" | "overlay";
  onCancel: () => void;
  onReload: () => void;
  onSaved: (file: PodTextFile) => void;
  onDirtyChange?: (dirty: boolean) => void;
  onSavingChange?: (saving: boolean) => void;
}) {
  const [contents, setContents] = useState(initial.contents);
  const [savedContents, setSavedContents] = useState(initial.contents);
  const [revision, setRevision] = useState(initial.revision);
  const [status, setStatus] = useState<SaveStatus>({ tag: "idle" });
  const [confirmCancel, setConfirmCancel] = useState(false);
  const [markdownRepresentation, setMarkdownRepresentation] =
    useState<MarkdownRepresentation>("source");
  const dirty = contents !== savedContents;
  const markdown = isMarkdownPath(String(path));
  const encodedSize = useMemo(() => new TextEncoder().encode(contents).byteLength, [contents]);
  const tooLarge = encodedSize > POD_TEXT_FILE_BYTE_LIMIT;

  useEffect(() => onDirtyChange?.(dirty), [dirty, onDirtyChange]);
  useEffect(
    () => onSavingChange?.(status.tag === "saving"),
    [onSavingChange, status.tag],
  );
  useEffect(() => {
    return () => {
      onDirtyChange?.(false);
      onSavingChange?.(false);
    };
  }, [onDirtyChange, onSavingChange]);
  useEffect(() => {
    if (!dirty) return;
    const warnBeforeUnload = (event: BeforeUnloadEvent) => {
      event.preventDefault();
      event.returnValue = "";
    };
    window.addEventListener("beforeunload", warnBeforeUnload);
    return () => window.removeEventListener("beforeunload", warnBeforeUnload);
  }, [dirty]);

  const save = useCallback(async () => {
    if (!dirty || tooLarge || status.tag === "saving") return;
    setStatus({ tag: "saving" });
    try {
      const nextRevision = await savePodTextFile(
        workspace,
        podId,
        root,
        path,
        contents,
        revision,
      );
      setSavedContents(contents);
      setRevision(nextRevision);
      setStatus({ tag: "saved" });
      onSaved({ contents, revision: nextRevision, writable: true });
    } catch (cause) {
      setStatus(
        cause instanceof PodFileConflictError
          ? { tag: "conflict" }
          : { tag: "error", message: errorMessage(cause) },
      );
    }
  }, [contents, dirty, onSaved, path, podId, revision, root, status.tag, tooLarge, workspace]);

  useEffect(() => {
    const saveShortcut = (event: KeyboardEvent) => {
      if (
        event.defaultPrevented
        || event.repeat
        || event.isComposing
        || (!event.metaKey && !event.ctrlKey)
        || event.altKey
        || event.shiftKey
        || event.key.toLowerCase() !== "s"
      ) return;
      event.preventDefault();
      void save();
    };
    window.addEventListener("keydown", saveShortcut);
    return () => window.removeEventListener("keydown", saveShortcut);
  }, [save]);

  const updateContents = (value: string) => {
    setContents(value);
    if (status.tag !== "idle") setStatus({ tag: "idle" });
  };
  const requestCancel = () => {
    if (dirty) setConfirmCancel(true);
    else onCancel();
  };

  return (
    <div className="flex h-full min-h-0 flex-col bg-[var(--syntax-background)]">
      <div className="flex min-h-11 shrink-0 items-center justify-between gap-3 border-b border-ui-border bg-surface px-3">
        <div className="min-w-0 flex-1 truncate text-[10px] text-subtle" role="status">
          {status.tag === "saving"
            ? "Saving…"
            : status.tag === "saved" && !dirty
              ? "Saved"
              : dirty
                ? "Unsaved changes"
                : "No changes"}
          {tooLarge ? <span className="ml-2 text-red-300">Maximum file size is 2 MiB.</span> : null}
        </div>
        <div className="flex shrink-0 items-center gap-2">
          {markdown ? (
            <SegmentedControl
              label="Markdown editor representation"
              options={MARKDOWN_REPRESENTATIONS}
              value={markdownRepresentation}
              onValueChange={setMarkdownRepresentation}
            />
          ) : null}
          <Button disabled={status.tag === "saving"} size="small" onClick={requestCancel}>
            Cancel
          </Button>
          <Button
            disabled={!dirty || tooLarge || status.tag === "saving"}
            size="small"
            title="Save (Ctrl/Command-S)"
            variant="primary"
            onClick={() => void save()}
          >
            {status.tag === "saving"
              ? <LoaderCircle aria-hidden="true" className="size-3.5 animate-spin" />
              : <Save aria-hidden="true" className="size-3.5" />}
            Save
          </Button>
        </div>
      </div>
      {status.tag === "conflict" ? (
        <div className="flex shrink-0 items-center gap-3 border-b border-amber-500/20 bg-amber-500/5 px-3 py-2 text-xs text-amber-200" role="alert">
          <AlertTriangle aria-hidden="true" className="size-4 shrink-0" />
          <span className="min-w-0 flex-1">This file changed on disk. Your draft was not overwritten.</span>
          <Button className="border-amber-500/25 text-amber-100" size="small" onClick={onReload}>
            <RotateCcw aria-hidden="true" className="size-3.5" /> Discard &amp; reload
          </Button>
        </div>
      ) : status.tag === "error" ? (
        <p className="shrink-0 border-b border-red-500/20 bg-red-500/5 px-3 py-2 text-xs text-red-200" role="alert">
          {status.message}
        </p>
      ) : null}
      <div className="min-h-0 flex-1">
        {markdown && markdownRepresentation === "rendered" ? (
          <Suspense fallback={<MarkdownFallback />}>
            <div className="h-full overflow-auto">
              <div className="min-h-full px-6 py-4">
                {renderMarkdown(contents, root, String(path))}
              </div>
            </div>
          </Suspense>
        ) : (
          <Suspense fallback={<EditorFallback />}>
            <LightweightCodeEditor
              line={line}
              path={String(path)}
              value={contents}
              onChange={updateContents}
              onSave={() => void save()}
            />
          </Suspense>
        )}
      </div>
      <ConfirmDialog
        confirmLabel="Discard changes"
        description="Your unsaved edits to this file will be lost."
        layer={confirmLayer}
        open={confirmCancel}
        pending={status.tag === "saving"}
        title="Discard unsaved changes?"
        onConfirm={onCancel}
        onOpenChange={setConfirmCancel}
      />
    </div>
  );
}

type SaveStatus =
  | { tag: "idle" }
  | { tag: "saving" }
  | { tag: "saved" }
  | { tag: "conflict" }
  | { tag: "error"; message: string };

function EditorFallback() {
  return (
    <p className="flex items-center gap-2 p-4 text-xs text-subtle" role="status">
      <LoaderCircle aria-hidden="true" className="size-3.5 animate-spin" /> Loading editor…
    </p>
  );
}

function MarkdownFallback() {
  return <p className="p-4 text-xs text-subtle">Rendering Markdown…</p>;
}

function errorMessage(cause: unknown): string {
  return cause instanceof Error ? cause.message : String(cause);
}
