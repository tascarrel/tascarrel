import { FileDiff } from "lucide-react";
import { useEffect, useState } from "react";

import type { shares, workspaces } from "../../api/generated/index.ts";
import { Badge } from "../../components/ui/Badge.tsx";
import { ChangedFileRow } from "./changePresentation.tsx";
import {
  OverlayActionButtons,
  overlayChangeCountLabel,
  OverlayDecisionDialog,
  OverlayResolutionNotice,
} from "./OverlayApprovalControls.tsx";
import {
  OverlayChangeDetail,
  OverlayChangeMetadata,
  OverlayChangeSetSummary,
} from "./OverlayChangeDetails.tsx";
import { useOverlayDecision } from "./overlayDecision.ts";
import { overlayChangeKind, shortOverlayRevision } from "./overlayModel.ts";

export function OverlayChangesReview({
  workspace,
  approval,
}: {
  workspace: workspaces.WorkspaceName;
  approval: shares.ShareOverlayApprovalRequest;
}) {
  const [selectedPath, setSelectedPath] = useState<string>();
  const decision = useOverlayDecision(workspace, approval);
  const selectedChange = approval.changes.find((change) => change.path === selectedPath);

  useEffect(() => setSelectedPath(undefined), [approval.id]);

  return (
    <section className="flex min-h-0 flex-col overflow-hidden" aria-label={`Overlay changes in ${approval.share}`}>
      <header className="border-b border-ui-border px-4 py-3">
        <div className="flex flex-wrap items-start justify-between gap-3">
          <div className="min-w-0">
            <h2 className="truncate font-mono text-xs font-semibold">/mnt/{approval.share}</h2>
            <p className="mt-1 text-[10px] text-subtle">
              Overlay share · submitted {formatTimestamp(approval.createdAt)} · revision {shortOverlayRevision(approval.revision)}
            </p>
          </div>
          <div className="flex flex-wrap items-center gap-1.5">
            <Badge size="xs" tone="warning">Awaiting approval</Badge>
            <Badge size="xs" tone="muted">{overlayChangeCountLabel(approval.changes.length)}</Badge>
          </div>
        </div>
        <div className="mt-3 flex flex-wrap items-center justify-between gap-3">
          <p className="max-w-2xl text-[11px] leading-5 text-subtle">
            Approving writes this exact submitted revision to the host share after checking that its files have not changed independently.
          </p>
          <OverlayActionButtons decision={decision} />
        </div>
      </header>
      <OverlayResolutionNotice resolution={decision.resolution} error={decision.error} />
      <div className="grid min-h-0 flex-1 grid-cols-[minmax(13rem,18rem)_minmax(0,1fr)] overflow-hidden">
        <aside className="min-h-0 overflow-auto border-r border-ui-border p-2" aria-label="Changed overlay files">
          <button
            aria-pressed={!selectedPath}
            className="mb-1 flex w-full items-center gap-2 rounded-lg px-2.5 py-2 text-left text-[11px] text-muted outline-none hover:bg-surface focus-visible:outline-2 focus-visible:outline-accent data-[selected=true]:bg-surface-raised data-[selected=true]:text-foreground"
            data-selected={!selectedPath}
            type="button"
            onClick={() => setSelectedPath(undefined)}
          >
            <FileDiff aria-hidden="true" className="size-3.5" /> All changes
            <span className="ml-auto font-mono text-[10px] text-subtle">
              {String(approval.changes.length)}
            </span>
          </button>
          {approval.changes.map((change) => (
            <ChangedFileRow
              kind={overlayChangeKind(change)}
              key={change.path}
              metadata={<OverlayChangeMetadata change={change} compact />}
              path={change.path}
              selected={selectedPath === change.path}
              onSelect={() => setSelectedPath(change.path)}
            />
          ))}
        </aside>
        <div className="min-h-0 overflow-auto p-5">
          {selectedChange
            ? <OverlayChangeDetail change={selectedChange} />
            : <OverlayChangeSetSummary approval={approval} />}
        </div>
      </div>
      <OverlayDecisionDialog approval={approval} decision={decision} />
    </section>
  );
}

function formatTimestamp(value: string): string {
  const date = new Date(value);
  return Number.isNaN(date.valueOf()) ? value : date.toLocaleString();
}
