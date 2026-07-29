import {
  Check,
  GitBranch,
  LoaderCircle,
  Tag,
  X,
} from "lucide-react";
import { useId, useState } from "react";

import { hostApi } from "../../api/client.ts";
import type { pods, repositories, workspaces } from "../../api/generated/index.ts";
import { Badge } from "../../components/ui/Badge.tsx";
import { Button } from "../../components/ui/Button.tsx";
import { ConfirmDialog } from "../../components/ui/ConfirmDialog.tsx";
import { CountBadge } from "../../components/ui/CountBadge.tsx";
import {
  displayApprovalReference,
  formatApprovalUpdateCount,
} from "./approvalPresentation.ts";
import { RepositoryApprovalReview } from "./RepositoryApprovalReview.tsx";

type ApprovalDecision = "approve" | "reject";

export function MobileRepositoryApprovals({
  workspace,
  approvals,
  podTitlesById,
  loadError,
}: {
  workspace: workspaces.WorkspaceName;
  approvals: readonly repositories.RepositoryApprovalRequest[];
  podTitlesById?: ReadonlyMap<pods.PodId, string>;
  loadError?: string;
}) {
  const headingId = useId();
  const pendingApprovals = approvals.filter(
    (approval) => approval.status.tag === "Pending" || approval.status.tag === "Failed",
  );
  const [decisionTarget, setDecisionTarget] = useState<{
    approval: repositories.RepositoryApprovalRequest;
    decision: ApprovalDecision;
  }>();
  const [submittingId, setSubmittingId] = useState<repositories.RepositoryApprovalId>();
  const [error, setError] = useState<string>();
  const visibleError = error ?? loadError;

  if (!pendingApprovals.length && !visibleError) return null;

  const resolve = async (
    approval: repositories.RepositoryApprovalRequest,
    decision: repositories.RepositoryApprovalDecision,
  ) => {
    if (submittingId) return;
    setSubmittingId(approval.id);
    setError(undefined);
    try {
      await hostApi.execute("repositories_ResolveApproval", {
        workspace,
        approvalId: approval.id,
        decision,
      });
      setDecisionTarget(undefined);
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setSubmittingId(undefined);
    }
  };

  return (
    <section aria-labelledby={headingId}>
      <h2
        className="flex min-w-0 flex-1 items-center gap-2 text-xs font-semibold uppercase tracking-[0.08em] text-muted"
        id={headingId}
      >
        <span className="truncate">Publication Approvals</span>
        <CountBadge count={pendingApprovals.length} size="xs" tone="muted" />
      </h2>
      {visibleError ? (
        <p className="mt-3 rounded-xl border border-red-500/20 bg-red-500/5 p-3 text-xs leading-5 text-red-200" role="alert">
          {visibleError}
        </p>
      ) : null}
      <div className="mt-3 grid gap-3">
        {pendingApprovals.map((approval) => (
          <MobileRepositoryApprovalCard
            approval={approval}
            key={approval.id}
            podTitle={podTitlesById?.get(approval.podId) ?? "Unknown pod"}
            submitting={submittingId === approval.id}
            workspace={workspace}
            onDecide={(decision) => setDecisionTarget({ approval, decision })}
          />
        ))}
      </div>
      <ConfirmDialog
        confirmLabel={decisionTarget?.decision === "reject" ? "Reject publication" : "Approve publication"}
        description={decisionTarget
          ? `${decisionTarget.decision === "reject" ? "Reject" : "Approve"} ${formatApprovalUpdateCount(decisionTarget.approval.updates.length)} from /workspace/${decisionTarget.approval.path}?`
          : ""}
        destructive={decisionTarget?.decision === "reject"}
        open={decisionTarget !== undefined}
        pending={decisionTarget?.approval.id === submittingId}
        title={decisionTarget?.decision === "reject" ? "Reject Publication?" : "Approve Publication?"}
        onOpenChange={(open) => {
          if (!open) setDecisionTarget(undefined);
        }}
        onConfirm={() => {
          if (!decisionTarget) return;
          void resolve(
            decisionTarget.approval,
            decisionTarget.decision === "reject" ? { tag: "Reject" } : { tag: "Approve" },
          );
        }}
      />
    </section>
  );
}

function MobileRepositoryApprovalCard({
  workspace,
  approval,
  podTitle,
  submitting,
  onDecide,
}: {
  workspace: workspaces.WorkspaceName;
  approval: repositories.RepositoryApprovalRequest;
  podTitle: string;
  submitting: boolean;
  onDecide: (decision: ApprovalDecision) => void;
}) {
  const [reviewReady, setReviewReady] = useState(false);

  return (
    <article className="min-w-0 max-w-full overflow-hidden rounded-2xl border border-amber-500/25 bg-amber-500/[0.04]">
      <div className="p-4">
        <div className="flex items-start justify-between gap-3">
          <div className="min-w-0">
            <h3 className="truncate text-sm font-semibold text-foreground">{podTitle}</h3>
            <p className="mt-1 break-all font-mono text-[10px] leading-4 text-subtle">
              /workspace/{approval.path}
            </p>
          </div>
          <Badge tone={approval.status.tag === "Failed" ? "danger" : "warning"}>
            {approval.status.tag === "Failed" ? "Failed" : "Review"}
          </Badge>
        </div>
        <p className="mt-2 break-all text-[11px] leading-5 text-muted">
          Publish to <span className="font-mono text-foreground">{approval.source}</span>
        </p>
        {approval.status.tag === "Failed" ? (
          <p className="mt-2 text-xs leading-5 text-red-200" role="alert">
            {approval.status.content}
          </p>
        ) : null}
      </div>

      <ul className="list-none divide-y divide-ui-border border-y border-ui-border p-0">
        {approval.updates.map((update) => (
          <li className="px-4 py-3" key={update.reference}>
            <div className="flex items-start gap-2">
              {update.reference.startsWith("refs/tags/") ? (
                <Tag aria-hidden="true" className="mt-0.5 size-3.5 shrink-0 text-accent-text" />
              ) : (
                <GitBranch
                  aria-hidden="true"
                  className="mt-0.5 size-3.5 shrink-0 text-accent-text"
                />
              )}
              <span className="min-w-0 flex-1">
                <span className="block break-all font-mono text-xs text-foreground">
                  {displayApprovalReference(update.reference)}
                </span>
                <span className="mt-1 block break-all font-mono text-[9px] leading-4 text-subtle">
                  {shortHash(update.previousObject) ?? "new reference"} →{" "}
                  {shortHash(update.proposedObject)}
                </span>
              </span>
              {update.rewrites ? <Badge size="xs" tone="warning">Rewrite</Badge> : null}
            </div>
          </li>
        ))}
      </ul>

      <RepositoryApprovalReview
        approval={approval}
        workspace={workspace}
        onReadyChange={setReviewReady}
      />

      <div className="p-3">
        <div className="grid grid-cols-2 gap-2">
          <Button
            className="h-11"
            variant="danger"
            disabled={submitting}
            onClick={() => onDecide("reject")}
          >
            {submitting ? (
              <LoaderCircle aria-hidden="true" className="size-3.5 animate-spin" />
            ) : (
              <X aria-hidden="true" className="size-3.5" />
            )}
            Reject
          </Button>
          <Button
            className="h-11"
            variant="primary"
            disabled={submitting || approval.status.tag !== "Pending" || !reviewReady}
            onClick={() => onDecide("approve")}
          >
            {submitting ? (
              <LoaderCircle aria-hidden="true" className="size-3.5 animate-spin" />
            ) : (
              <Check aria-hidden="true" className="size-3.5" />
            )}
            Approve
          </Button>
        </div>
        {approval.status.tag === "Pending" && !reviewReady ? (
          <p className="mt-2 text-center text-[10px] leading-4 text-subtle" aria-live="polite">
            Commit review must load before approving.
          </p>
        ) : null}
      </div>
    </article>
  );
}

function shortHash(value: string | undefined): string | undefined {
  return value && value.length > 12 ? value.slice(0, 12) : value;
}
