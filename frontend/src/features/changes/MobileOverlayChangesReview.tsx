import { ArrowLeft } from "lucide-react";
import { useEffect, useState, type ReactNode } from "react";

import type { shares, workspaces } from "../../api/generated/index.ts";
import { Badge } from "../../components/ui/Badge.tsx";
import { ChangedFileRow } from "./changePresentation.tsx";
import {
  OverlayActionButtons,
  overlayChangeCountLabel,
  OverlayDecisionDialog,
  OverlayResolutionNotice,
} from "./OverlayApprovalControls.tsx";
import { OverlayChangeDetail, OverlayChangeMetadata } from "./OverlayChangeDetails.tsx";
import { useOverlayDecision } from "./overlayDecision.ts";
import { overlayChangeKind, shortOverlayRevision } from "./overlayModel.ts";

export function MobileOverlayChangesReview({
  workspace,
  approval,
  sourceNotice,
  sourcePicker,
}: {
  workspace: workspaces.WorkspaceName;
  approval: shares.ShareOverlayApprovalRequest;
  sourceNotice?: ReactNode;
  sourcePicker?: ReactNode;
}) {
  const [selectedPath, setSelectedPath] = useState<string>();
  const decision = useOverlayDecision(workspace, approval);
  const selectedChange = approval.changes.find((change) => change.path === selectedPath);

  useEffect(() => setSelectedPath(undefined), [approval.id]);

  if (selectedChange) {
    return (
      <div className="flex h-full min-h-0 flex-col overflow-hidden bg-canvas">
        <header className="mobile-client-horizontal flex min-h-14 items-center gap-2 border-b border-ui-border">
          <button
            aria-label="Changed file list"
            className="flex size-11 shrink-0 items-center justify-center rounded-xl text-muted outline-none active:bg-surface-raised focus-visible:outline-2 focus-visible:outline-accent"
            type="button"
            onClick={() => setSelectedPath(undefined)}
          >
            <ArrowLeft aria-hidden="true" className="size-5" />
          </button>
          <div className="min-w-0 flex-1">
            <h2 className="truncate font-mono text-xs font-semibold text-foreground">{selectedChange.path}</h2>
            <p className="mt-0.5 truncate text-[10px] text-subtle">/mnt/{approval.share}</p>
          </div>
        </header>
        <div className="min-h-0 flex-1 overflow-auto p-5">
          <OverlayChangeDetail change={selectedChange} />
        </div>
      </div>
    );
  }

  return (
    <div className="mobile-client-content h-full min-h-0 overflow-y-auto pt-4">
      <div className="mx-auto w-full min-w-0 max-w-2xl pb-5">
        {sourcePicker}
        <div className={`${sourcePicker ? "mt-3" : ""} rounded-xl border border-ui-border bg-surface/70 p-3`}>
          <p className="truncate font-mono text-xs font-semibold text-foreground">/mnt/{approval.share}</p>
          <p className="mt-1 text-[10px] text-subtle">
            Overlay share · revision {shortOverlayRevision(approval.revision)}
          </p>
        </div>
        {sourceNotice}

        <div className="mt-3 flex flex-wrap items-center gap-2">
          <Badge tone="warning">Awaiting approval</Badge>
          <Badge tone="muted">{overlayChangeCountLabel(approval.changes.length)}</Badge>
        </div>
        <OverlayResolutionNotice resolution={decision.resolution} error={decision.error} mobile />

        <div className="mt-5 grid gap-2">
          {approval.changes.map((change) => (
            <ChangedFileRow
              kind={overlayChangeKind(change)}
              key={change.path}
              metadata={<OverlayChangeMetadata change={change} />}
              mobile
              path={change.path}
              onSelect={() => setSelectedPath(change.path)}
            />
          ))}
        </div>

        <div className="mt-5 border-t border-ui-border pt-4">
          <p className="text-xs leading-5 text-subtle">
            Approving writes this exact submitted revision to the host share after checking that its files have not changed independently.
          </p>
          <div className="mt-3">
            <OverlayActionButtons decision={decision} />
          </div>
        </div>
      </div>
      <OverlayDecisionDialog approval={approval} decision={decision} />
    </div>
  );
}
