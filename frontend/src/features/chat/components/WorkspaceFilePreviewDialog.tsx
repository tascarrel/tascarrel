import { Dialog } from "@base-ui/react/dialog";
import { Download, X } from "lucide-react";
import { useState } from "react";

import { workspaceFileUrl } from "../../../api/files.ts";
import type { files, pods, workspaces } from "../../../api/generated/index.ts";
import { SegmentedControl } from "../../../components/ui/SegmentedControl.tsx";
import {
  isMarkdownPath,
  MARKDOWN_REPRESENTATIONS,
  type MarkdownRepresentation,
  type WorkspaceMarkdownRenderer,
  WorkspaceFileViewer,
} from "../../files/WorkspaceFileViewer.tsx";
import type { WorkspaceFileTarget } from "../model/workspaceFileLinks.ts";

export function WorkspaceFilePreviewDialog({
  workspace,
  podId,
  target,
  renderMarkdown,
  onClose,
}: {
  workspace: workspaces.WorkspaceName;
  podId: pods.PodId;
  target: WorkspaceFileTarget;
  renderMarkdown: WorkspaceMarkdownRenderer;
  onClose: () => void;
}) {
  const [markdownRepresentation, setMarkdownRepresentation] =
    useState<MarkdownRepresentation>(target.line ? "source" : "rendered");
  const path = target.path as files.FilePath;
  const markdown = isMarkdownPath(target.path);
  const title = `/workspace/${target.path}${target.line ? `:${target.line}` : ""}`;

  return (
    <Dialog.Root open onOpenChange={(open) => !open && onClose()}>
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
              {markdown ? (
                <SegmentedControl
                  label="Markdown representation"
                  options={MARKDOWN_REPRESENTATIONS}
                  value={markdownRepresentation}
                  onValueChange={setMarkdownRepresentation}
                />
              ) : null}
              <a
                aria-label={`Download ${target.path}`}
                className="grid size-8 shrink-0 place-items-center rounded-lg border border-ui-border/70 bg-surface text-muted outline-none transition hover:border-ui-border-strong hover:bg-surface-raised hover:text-foreground focus-visible:outline-2 focus-visible:outline-accent"
                download={fileName(target.path)}
                href={workspaceFileUrl(workspace, podId, path, true)}
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
              <WorkspaceFileViewer
                line={target.line}
                markdownRepresentation={markdownRepresentation}
                path={path}
                podId={podId}
                renderMarkdown={renderMarkdown}
                workspace={workspace}
              />
            </div>
          </Dialog.Popup>
        </Dialog.Viewport>
      </Dialog.Portal>
    </Dialog.Root>
  );
}

function fileName(path: string): string {
  return path.slice(path.lastIndexOf("/") + 1) || "file";
}
