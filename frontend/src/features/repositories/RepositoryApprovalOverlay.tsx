import { AlertDialog } from "@base-ui/react/alert-dialog";
import { AlertTriangle, GitBranch, Tag } from "lucide-react";
import { useState } from "react";

import { hostApi } from "../../api/client.ts";
import type { pods, repositories, workspaces } from "../../api/generated/index.ts";
import { Button } from "../../components/ui/Button.tsx";
import {
  approvalReferenceKind,
  displayApprovalReference,
  formatApprovalUpdateCount,
} from "./approvalPresentation.ts";
import { RepositoryApprovalReview } from "./RepositoryApprovalReview.tsx";

export function RepositoryApprovalOverlay({
  workspace,
  approvals,
  podTitlesById,
  onError,
}: {
  workspace: workspaces.WorkspaceName;
  approvals: readonly repositories.RepositoryApprovalRequest[];
  podTitlesById?: ReadonlyMap<pods.PodId, string>;
  onError: (cause: unknown) => void;
}) {
  const approval = approvals.find(
    (candidate) => candidate.status.tag === "Pending" && !candidate.postponed,
  );

  const resolve = async (
    approval: repositories.RepositoryApprovalRequest,
    decision: repositories.RepositoryApprovalDecision,
  ) => {
    try {
      await hostApi.execute("repositories_ResolveApproval", {
        workspace,
        approvalId: approval.id,
        decision,
      });
    } catch (cause) {
      onError(cause);
      throw cause;
    }
  };

  if (!approval) return null;

  return (
    <RepositoryApprovalDialog
      approval={approval}
      key={approval.id}
      podTitle={podTitlesById?.get(approval.podId) ?? "Unknown pod"}
      workspace={workspace}
      onApprove={() => resolve(approval, { tag: "Approve" })}
      onPostpone={() => resolve(approval, { tag: "Postpone" })}
      onReject={() => resolve(approval, { tag: "Reject" })}
    />
  );
}

function RepositoryApprovalDialog({
  approval,
  podTitle,
  workspace,
  onApprove,
  onPostpone,
  onReject,
}: {
  approval: repositories.RepositoryApprovalRequest;
  podTitle: string;
  workspace: workspaces.WorkspaceName;
  onApprove: () => Promise<void>;
  onPostpone: () => Promise<void>;
  onReject: () => Promise<void>;
}) {
  const [reviewReady, setReviewReady] = useState(false);
  const [submitting, setSubmitting] = useState(false);

  const submit = (action: () => Promise<void>) => {
    if (submitting) return;
    setSubmitting(true);
    void action().catch(() => setSubmitting(false));
  };

  return (
    <AlertDialog.Root
      open
      onOpenChange={(open) => {
        if (!open) submit(onPostpone);
      }}
    >
      <AlertDialog.Portal>
        <AlertDialog.Backdrop className="fixed inset-0 z-[90] bg-black/75 backdrop-blur-sm transition-opacity data-[ending-style]:opacity-0 data-[starting-style]:opacity-0" />
        <AlertDialog.Viewport className="fixed inset-0 z-[90] grid place-items-center overflow-y-auto p-4">
          <AlertDialog.Popup className="flex max-h-[min(54rem,calc(100dvh-2rem))] w-full max-w-6xl flex-col overflow-hidden rounded-2xl border border-ui-border-strong bg-surface-raised text-foreground shadow-2xl shadow-black/70 outline-none transition-[transform,opacity] data-[ending-style]:scale-95 data-[ending-style]:opacity-0 data-[starting-style]:scale-95 data-[starting-style]:opacity-0">
            <div className="border-b border-ui-border px-5 py-4">
              <AlertDialog.Title className="text-base font-semibold">
                Approve Repository Publication?
              </AlertDialog.Title>
              <AlertDialog.Description className="mt-1.5 text-sm leading-5 text-muted">
                <span className="font-medium text-foreground">{podTitle}</span> wants to publish{" "}
                {formatApprovalUpdateCount(approval.updates.length)} from{" "}
                <span className="font-mono text-xs text-foreground">
                  /workspace/{approval.path}
                </span>.
              </AlertDialog.Description>
            </div>

            <div className="min-h-0 overflow-y-auto">
              <dl className="grid grid-cols-[auto_minmax(0,1fr)] gap-x-4 gap-y-1 border-b border-ui-border px-5 py-3 text-xs">
                <dt className="text-subtle">Pod</dt>
                <dd className="truncate text-muted" title={String(approval.podId)}>
                  {podTitle} <span className="font-mono text-[10px] text-subtle">({shortId(approval.podId)})</span>
                </dd>
                <dt className="text-subtle">Upstream</dt>
                <dd className="truncate font-mono text-[10px] text-muted" title={approval.source}>
                  {approval.source}
                </dd>
              </dl>

              <ul className="list-none divide-y divide-ui-border p-0">
                {approval.updates.map((update) => (
                  <li className="px-5 py-3.5" key={update.reference}>
                    <div className="flex items-start justify-between gap-3">
                      <div className="min-w-0">
                        <p className="flex items-center gap-2 text-sm font-medium text-foreground">
                          {update.reference.startsWith("refs/tags/")
                            ? <Tag aria-hidden="true" className="size-4 shrink-0 text-accent-text" />
                            : <GitBranch aria-hidden="true" className="size-4 shrink-0 text-accent-text" />}
                          <span>{approvalReferenceKind(update.reference)}</span>
                          <span className="break-all font-mono text-xs">
                            {displayApprovalReference(update.reference)}
                          </span>
                        </p>
                        <p className="mt-1 break-all font-mono text-[10px] leading-4 text-subtle">
                          {update.reference}
                        </p>
                      </div>
                      {update.rewrites ? (
                        <span className="flex shrink-0 items-center gap-1 text-[10px] font-medium text-amber-200">
                          <AlertTriangle aria-hidden="true" className="size-3.5" />
                          Rewrite
                        </span>
                      ) : null}
                    </div>
                    <dl className="mt-2 grid grid-cols-[auto_minmax(0,1fr)] gap-x-3 gap-y-1 font-mono text-[10px] leading-4">
                      <dt className="font-sans text-subtle">From</dt>
                      <dd className="break-all text-muted">{update.previousObject ?? "new reference"}</dd>
                      <dt className="font-sans text-subtle">To</dt>
                      <dd className="break-all text-muted">{update.proposedObject}</dd>
                    </dl>
                  </li>
                ))}
              </ul>
              <RepositoryApprovalReview
                approval={approval}
                workspace={workspace}
                onReadyChange={setReviewReady}
              />
            </div>

            <div className="border-t border-ui-border bg-canvas/40 px-5 py-4">
              <p className="mb-3 text-[11px] leading-4 text-subtle">
                {reviewReady
                  ? "Approve publishes the exact references and commits shown above."
                  : "Commit review must load successfully before this publication can be approved."}
              </p>
              <div className="flex flex-wrap justify-end gap-2">
                <Button autoFocus disabled={submitting} onClick={() => submit(onPostpone)}>
                  Postpone
                </Button>
                <Button
                  disabled={submitting}
                  variant="danger"
                  onClick={() => submit(onReject)}
                >
                  Reject
                </Button>
                <Button
                  variant="primary"
                  disabled={!reviewReady || submitting}
                  onClick={() => submit(onApprove)}
                >
                  Approve
                </Button>
              </div>
            </div>
          </AlertDialog.Popup>
        </AlertDialog.Viewport>
      </AlertDialog.Portal>
    </AlertDialog.Root>
  );
}

function shortId(id: unknown): string {
  const value = String(id);
  const suffix = value.split("_").at(-1) ?? value;
  return suffix.slice(0, 8);
}
