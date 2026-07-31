import { AlertTriangle, Trash2 } from "lucide-react";

import type { pods } from "../../api/generated/index.ts";
import { ConfirmDialog } from "../../components/ui/ConfirmDialog.tsx";
import type { PodChangeSummary } from "../changes/podChangeSummary.ts";

export function PodDestructionDialog({
  pod,
  summary,
  verified,
  pending,
  error,
  action = "destroy",
  onOpenChange,
  onConfirm,
}: {
  pod?: pods.Pod;
  summary?: PodChangeSummary;
  verified: boolean;
  pending: boolean;
  error?: string;
  action?: "destroy" | "delete";
  onOpenChange: (open: boolean) => void;
  onConfirm: () => void;
}) {
  const actionLabel = action === "delete" ? "Delete pod" : "Destroy pod";
  return (
    <ConfirmDialog
      confirmLabel={actionLabel}
      description={pod ? (
        <PodDestructionWarning
          error={error}
          pod={pod}
          summary={summary}
          verified={verified}
        />
      ) : null}
      destructive
      open={Boolean(pod)}
      pending={pending}
      title={pod
        ? <PodDestructionTitle actionLabel={actionLabel} pod={pod} />
        : actionLabel}
      onOpenChange={onOpenChange}
      onConfirm={onConfirm}
    />
  );
}

function PodDestructionTitle({
  actionLabel,
  pod,
}: {
  actionLabel: string;
  pod: pods.Pod;
}) {
  return (
    <span className="flex min-w-0 items-center gap-3">
      <span className="grid size-9 shrink-0 place-items-center rounded-full bg-red-500/10 text-red-300">
        <Trash2 aria-hidden="true" className="size-4" />
      </span>
      <span className="min-w-0">
        <span className="block text-xs font-medium text-red-300">{actionLabel}</span>
        <span className="mt-0.5 block truncate text-lg font-semibold text-foreground">
          {pod.title || "Untitled pod"}
        </span>
      </span>
    </span>
  );
}

function PodDestructionWarning({
  pod,
  summary,
  verified,
  error,
}: {
  pod: pods.Pod;
  summary?: PodChangeSummary;
  verified: boolean;
  error?: string;
}) {
  const hasLocalWork = Boolean(
    summary?.changedFileCount || summary?.overlayChangeCount || summary?.unpushedCommitCount,
  );
  const statusWarnings = repositoryStatusWarnings(summary, verified);
  return (
    <span className="block">
      <span className="block break-all font-mono text-[10px] leading-4 text-subtle">
        {pod.id}
      </span>

      {hasLocalWork ? (
        <span className="mt-4 block rounded-xl border border-amber-500/25 bg-amber-500/[0.07] p-4 text-amber-100">
          <span className="flex items-center gap-2 text-xs font-semibold text-amber-200">
            <AlertTriangle aria-hidden="true" className="size-4 shrink-0" />
            Local work will be lost
          </span>
          <span className="mt-3 grid grid-cols-2 gap-4">
            {summary?.changedFileCount ? (
              <span className="block">
                <strong className="block text-xl font-semibold leading-none tabular-nums text-amber-100">
                  {summary.changedFileCount}
                </strong>
                <span className="mt-1.5 block text-[11px] leading-4 text-amber-200/75">
                  uncommitted file {summary.changedFileCount === 1 ? "change" : "changes"}
                </span>
              </span>
            ) : null}
            {summary?.unpushedCommitCount ? (
              <span className="block">
                <strong className="block text-xl font-semibold leading-none tabular-nums text-amber-100">
                  {summary.unpushedCommitCount}
                </strong>
                <span className="mt-1.5 block text-[11px] leading-4 text-amber-200/75">
                  unpushed {summary.unpushedCommitCount === 1 ? "commit" : "commits"}
                </span>
              </span>
            ) : null}
            {summary?.overlayChangeCount ? (
              <span className="block">
                <strong className="block text-xl font-semibold leading-none tabular-nums text-amber-100">
                  {summary.overlayChangeCount}
                </strong>
                <span className="mt-1.5 block text-[11px] leading-4 text-amber-200/75">
                  overlay {summary.overlayChangeCount === 1 ? "change" : "changes"} awaiting approval
                </span>
              </span>
            ) : null}
          </span>
          {summary?.conflictCount ? (
            <span className="mt-3 block text-[11px] leading-4 text-red-300">
              Includes {summary.conflictCount} unresolved {summary.conflictCount === 1 ? "conflict" : "conflicts"}.
            </span>
          ) : null}
        </span>
      ) : null}

      {statusWarnings.length > 0 ? (
        <span className="mt-3 block border-l-2 border-amber-400 bg-amber-500/[0.05] px-3 py-2.5 text-xs leading-5 text-amber-200">
          {statusWarnings.join(" ")}
        </span>
      ) : null}

      {error ? (
        <span
          className="mt-3 block [overflow-wrap:anywhere] rounded-xl border border-red-500/20 bg-red-500/5 px-3 py-2.5 text-xs leading-5 text-red-200"
          role="alert"
        >
          {error}
        </span>
      ) : null}

      <span className="mt-4 block text-xs leading-5 text-muted">
        This permanently deletes the pod and all of its persistent resources.
        <strong className="ml-1 font-semibold text-red-300">This cannot be undone.</strong>
      </span>
    </span>
  );
}

function repositoryStatusWarnings(
  summary: PodChangeSummary | undefined,
  verified: boolean,
): string[] {
  const warnings: string[] = [];
  if (!verified) {
    warnings.push("Repository status is not current, so local work could not be fully verified.");
  }
  if (summary?.repositoryWithoutUpstreamCount) {
    warnings.push(
      `Push status is unavailable for ${summary.repositoryWithoutUpstreamCount} ${summary.repositoryWithoutUpstreamCount === 1 ? "repository" : "repositories"} without an upstream.`,
    );
  }
  if (summary?.inspectionFailureCount) {
    warnings.push(
      `${summary.inspectionFailureCount} ${summary.inspectionFailureCount === 1 ? "repository could" : "repositories could"} not be inspected.`,
    );
  }
  return warnings;
}
