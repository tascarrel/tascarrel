import { Dialog } from "@base-ui/react/dialog";
import { Download, Pencil, X } from "lucide-react";
import { useState } from "react";

import { podFilePath, podFileUrl } from "../../../api/files.ts";
import type { files, pods, workspaces } from "../../../api/generated/index.ts";
import { Button } from "../../../components/ui/Button.tsx";
import { ConfirmDialog } from "../../../components/ui/ConfirmDialog.tsx";
import { SegmentedControl } from "../../../components/ui/SegmentedControl.tsx";
import { PodFileEditor } from "../../files/PodFileEditor.tsx";
import {
  isMarkdownPath,
  MARKDOWN_REPRESENTATIONS,
  type MarkdownRepresentation,
  type PodMarkdownRenderer,
  type PodTextFile,
  PodFileViewer,
} from "../../files/PodFileViewer.tsx";
import type { PodFileTarget } from "../model/podFileLinks.ts";

export function PodFilePreviewDialog({
  workspace,
  podId,
  target,
  renderMarkdown,
  onClose,
}: {
  workspace: workspaces.WorkspaceName;
  podId: pods.PodId;
  target: PodFileTarget;
  renderMarkdown: PodMarkdownRenderer;
  onClose: () => void;
}) {
  const [markdownRepresentation, setMarkdownRepresentation] =
    useState<MarkdownRepresentation>(target.line ? "source" : "rendered");
  const [textFile, setTextFile] = useState<PodTextFile>();
  const [editing, setEditing] = useState(false);
  const [editorDirty, setEditorDirty] = useState(false);
  const [editorSaving, setEditorSaving] = useState(false);
  const [confirmClose, setConfirmClose] = useState(false);
  const [revision, setRevision] = useState(0);
  const path = target.path as files.FilePath;
  const markdown = isMarkdownPath(target.path);
  const title = `${podFilePath(target.root, target.path)}${target.line ? `:${target.line}` : ""}`;

  return (
    <Dialog.Root
      open
      onOpenChange={(open) => {
        if (open) return;
        if (editorDirty || editorSaving) setConfirmClose(true);
        else onClose();
      }}
    >
      <Dialog.Portal>
        <Dialog.Backdrop className="fixed inset-0 z-[70] bg-black/85 backdrop-blur-sm transition-opacity data-[ending-style]:opacity-0 data-[starting-style]:opacity-0" />
        <Dialog.Viewport className="fixed inset-0 z-[70] grid place-items-center overflow-hidden p-3 sm:p-6">
          <Dialog.Popup className="flex h-[min(88dvh,64rem)] w-full max-w-7xl flex-col overflow-hidden rounded-2xl border border-ui-border-strong bg-surface-raised text-foreground shadow-2xl shadow-black/70 outline-none transition-[transform,opacity] data-[ending-style]:scale-[0.98] data-[ending-style]:opacity-0 data-[starting-style]:scale-[0.98] data-[starting-style]:opacity-0">
            <div className="flex min-h-12 shrink-0 items-center gap-3 border-b border-ui-border px-3 sm:px-4">
              <Dialog.Title
                className="min-w-0 flex-1 truncate font-mono text-xs font-medium"
                title={title}
              >
                {title}
              </Dialog.Title>
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
                <Button aria-label={`Edit ${target.path}`} size="icon" title="Edit file" onClick={() => setEditing(true)}>
                  <Pencil aria-hidden="true" className="size-3.5" />
                </Button>
              ) : null}
              <a
                aria-label={`Download ${target.path}`}
                className="grid size-8 shrink-0 place-items-center rounded-lg border border-ui-border/70 bg-surface text-muted outline-none transition hover:border-ui-border-strong hover:bg-surface-raised hover:text-foreground focus-visible:outline-2 focus-visible:outline-accent"
                download={fileName(target.path)}
                href={podFileUrl(workspace, podId, target.root, path, true)}
                title="Download file"
              >
                <Download aria-hidden="true" className="size-3.5" />
              </a>
              <Dialog.Close
                aria-label="Close file preview"
                className="grid size-8 shrink-0 place-items-center rounded-lg text-muted outline-none transition hover:bg-surface hover:text-foreground focus-visible:outline-2 focus-visible:outline-accent"
              >
                <X aria-hidden="true" className="size-4" />
              </Dialog.Close>
            </div>
            <Dialog.Description className="sr-only">
              Preview of {title}
            </Dialog.Description>
            <div className="min-h-0 flex-1 overflow-hidden">
              {editing && textFile ? (
                <PodFileEditor
                  confirmLayer="overlay"
                  initial={textFile}
                  line={target.line}
                  path={path}
                  podId={podId}
                  renderMarkdown={renderMarkdown}
                  root={target.root}
                  workspace={workspace}
                  onCancel={() => {
                    setEditing(false);
                    setEditorDirty(false);
                    setEditorSaving(false);
                  }}
                  onDirtyChange={setEditorDirty}
                  onSavingChange={setEditorSaving}
                  onReload={() => {
                    setEditing(false);
                    setTextFile(undefined);
                    setRevision((current) => current + 1);
                    setEditorDirty(false);
                    setEditorSaving(false);
                  }}
                  onSaved={(file) => {
                    setTextFile(file);
                    setRevision((current) => current + 1);
                  }}
                />
              ) : (
                <PodFileViewer
                  line={target.line}
                  markdownRepresentation={markdownRepresentation}
                  path={path}
                  podId={podId}
                  renderMarkdown={renderMarkdown}
                  revision={revision}
                  root={target.root}
                  workspace={workspace}
                  onTextFile={setTextFile}
                />
              )}
            </div>
          </Dialog.Popup>
        </Dialog.Viewport>
      </Dialog.Portal>
      <ConfirmDialog
        confirmLabel={editorDirty ? "Discard changes" : "Close preview"}
        description={editorSaving
          ? "Wait for the current save to finish before closing this preview."
          : editorDirty
            ? "Your unsaved file edits will be lost when you close this preview."
            : "The save finished. Close this preview?"}
        layer="overlay"
        open={confirmClose}
        pending={editorSaving}
        title="Discard unsaved changes?"
        onConfirm={onClose}
        onOpenChange={setConfirmClose}
      />
    </Dialog.Root>
  );
}

function fileName(path: string): string {
  return path.slice(path.lastIndexOf("/") + 1) || "file";
}
