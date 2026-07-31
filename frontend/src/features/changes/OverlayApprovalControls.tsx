import { LoaderCircle } from "lucide-react";
import type { ReactNode } from "react";

import type { shares } from "../../api/generated/index.ts";
import { Button } from "../../components/ui/Button.tsx";
import { ConfirmDialog } from "../../components/ui/ConfirmDialog.tsx";
import { DiffViewer } from "../../components/ui/DiffViewer.tsx";
import type { OverlayDecisionState } from "./overlayDecision.ts";
import { shortOverlayRevision } from "./overlayModel.ts";

export function OverlayActionButtons({ decision }: { decision: OverlayDecisionState }) {
  const stale = decision.resolution?.result === "RevisionChanged";
  const resolved = decision.resolution?.result === "Applied"
    || decision.resolution?.result === "Rejected";
  return (
    <div className="flex shrink-0 justify-end gap-2">
      <Button
        disabled={decision.resolving || resolved}
        variant="danger"
        onClick={() => decision.setPendingDecision("Reject")}
      >
        Reject
      </Button>
      <Button
        disabled={decision.resolving || stale || resolved}
        variant="primary"
        onClick={() => decision.setPendingDecision("Approve")}
      >
        {decision.resolving && decision.pendingDecision === "Approve"
          ? <LoaderCircle aria-hidden="true" className="size-3.5 animate-spin" />
          : null}
        Approve
      </Button>
    </div>
  );
}

export function OverlayDecisionDialog({
  approval,
  decision,
}: {
  approval: shares.ShareOverlayApprovalRequest;
  decision: OverlayDecisionState;
}) {
  const rejecting = decision.pendingDecision === "Reject";
  return (
    <ConfirmDialog
      confirmLabel={rejecting ? "Reject changes" : "Approve changes"}
      description={rejecting
        ? <>Reject the submitted revision for <span className="font-mono text-xs text-foreground">/mnt/{approval.share}</span>? The pod will need to submit another request before any overlay changes can be applied.</>
        : <>Apply {overlayChangeCountLabel(approval.changes.length)} from revision <span className="font-mono text-xs text-foreground">{shortOverlayRevision(approval.revision)}</span> to the host share? Tascarrel checks the current host entries before writing anything.</>}
      destructive={rejecting}
      open={decision.pendingDecision !== undefined}
      pending={decision.resolving}
      title={rejecting ? "Reject overlay changes?" : "Approve overlay changes?"}
      onConfirm={() => void decision.resolve()}
      onOpenChange={(open) => {
        if (!open) decision.setPendingDecision(undefined);
      }}
    />
  );
}

export function OverlayResolutionNotice({
  resolution,
  error,
  mobile = false,
}: {
  resolution?: shares.ShareOverlayApprovalResolution;
  error?: string;
  mobile?: boolean;
}) {
  if (error) {
    return <Notice mobile={mobile} tone="danger">{error}</Notice>;
  }
  if (!resolution) return null;
  if (resolution.result === "Applied") {
    return <Notice mobile={mobile} tone="success">The overlay revision was applied.</Notice>;
  }
  if (resolution.result === "Rejected") {
    return <Notice mobile={mobile} tone="success">The overlay revision was rejected.</Notice>;
  }
  if (resolution.result === "RevisionChanged") {
    return (
      <Notice mobile={mobile} tone="danger">
        The pod changed this overlay after submitting it. This request cannot be approved; reject it and ask the pod to submit the current revision.
      </Notice>
    );
  }
  return (
    <div className={`${mobile ? "mt-3" : "border-b"} border-red-500/20 bg-red-500/5 px-4 py-3 text-xs text-red-100`} role="alert">
      <p className="font-medium">Host conflicts prevented the overlay from being applied.</p>
      <ul className="mt-2 list-disc space-y-2 pl-4 text-[11px] leading-5 text-red-200">
        {resolution.conflicts.map((conflict) => (
          <li key={conflict.path}>
            <span className="font-mono">{conflict.path}</span>: {conflict.reason}
            {conflict.textDiff ? (
              <div className="mt-2 max-h-72 overflow-auto rounded-lg border border-ui-border bg-canvas text-foreground">
                <DiffViewer patch={conflict.textDiff} fileName={conflict.path} />
              </div>
            ) : null}
          </li>
        ))}
      </ul>
    </div>
  );
}

export function overlayChangeCountLabel(count: number): string {
  return `${String(count)} ${count === 1 ? "change" : "changes"}`;
}

function Notice({
  children,
  mobile,
  tone,
}: {
  children: ReactNode;
  mobile: boolean;
  tone: "danger" | "success";
}) {
  return (
    <p
      className={`${mobile ? "mt-3 rounded-xl border" : "border-b"} px-4 py-3 text-xs leading-5 ${
        tone === "danger"
          ? "border-red-500/20 bg-red-500/5 text-red-200"
          : "border-emerald-500/20 bg-emerald-500/5 text-emerald-200"
      }`}
      role={tone === "danger" ? "alert" : "status"}
    >
      {children}
    </p>
  );
}
