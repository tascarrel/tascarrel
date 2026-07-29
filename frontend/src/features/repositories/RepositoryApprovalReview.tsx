import { AlertTriangle, LoaderCircle } from "lucide-react";
import { useEffect, useRef, useState } from "react";

import { hostApi } from "../../api/client.ts";
import type {
  changes,
  repositories,
  workspaces,
} from "../../api/generated/index.ts";
import { DiffViewer } from "../../components/ui/DiffViewer.tsx";
import {
  approvalReferenceKind,
  displayApprovalReference,
} from "./approvalPresentation.ts";

type ReviewLoad =
  | { status: "loading" }
  | { status: "error"; message: string }
  | { status: "ready"; result: repositories.RepositoryApprovalReviewResult };

type CommitSelection = {
  reference: string;
  commit: changes.GitCommit;
};

type CommitChangesLoad =
  | { status: "loading"; selection: CommitSelection }
  | { status: "error"; selection: CommitSelection; message: string }
  | {
      status: "ready";
      selection: CommitSelection;
      result: repositories.RepositoryApprovalCommitChangesResult;
    };

export function RepositoryApprovalReview({
  workspace,
  approval,
  onReadyChange,
}: {
  workspace: workspaces.WorkspaceName;
  approval: repositories.RepositoryApprovalRequest;
  onReadyChange?: (ready: boolean) => void;
}) {
  const [review, setReview] = useState<ReviewLoad>({ status: "loading" });
  const [changes, setChanges] = useState<CommitChangesLoad>();
  const changesRequest = useRef<AbortController | undefined>(undefined);

  useEffect(() => {
    const controller = new AbortController();
    changesRequest.current?.abort();
    setReview({ status: "loading" });
    setChanges(undefined);
    onReadyChange?.(false);
    void hostApi.execute("repositories_GetApprovalReview", {
      workspace,
      approvalId: approval.id,
    }, controller.signal).then(
      (output) => {
        setReview({ status: "ready", result: output.result });
        onReadyChange?.(output.result.status === "Review");
      },
      (cause) => {
        if (controller.signal.aborted) return;
        setReview({ status: "error", message: errorMessage(cause) });
        onReadyChange?.(false);
      },
    );
    return () => controller.abort();
  }, [approval.id, onReadyChange, workspace]);

  useEffect(() => () => changesRequest.current?.abort(), []);

  const loadCommitChanges = (selection: CommitSelection) => {
    changesRequest.current?.abort();
    const controller = new AbortController();
    changesRequest.current = controller;
    setChanges({ status: "loading", selection });
    void hostApi.execute("repositories_GetApprovalCommitChanges", {
      workspace,
      approvalId: approval.id,
      reference: selection.reference,
      commit: selection.commit.id,
    }, controller.signal).then(
      (output) => {
        if (!controller.signal.aborted) {
          setChanges({ status: "ready", selection, result: output.result });
        }
      },
      (cause) => {
        if (!controller.signal.aborted) {
          setChanges({
            status: "error",
            selection,
            message: errorMessage(cause),
          });
        }
      },
    );
  };

  if (review.status === "loading") {
    return (
      <p className="flex items-center gap-2 border-y border-ui-border px-4 py-4 text-xs text-subtle" aria-live="polite">
        <LoaderCircle aria-hidden="true" className="size-4 animate-spin" />
        Loading commits retained for this approval…
      </p>
    );
  }
  if (review.status === "error") {
    return <ReviewError message={`Could not load approval commits: ${review.message}`} />;
  }
  if (review.result.status === "TooLarge") {
    return (
      <ReviewError
        message={`Commit metadata exceeds the ${formatBytes(review.result.maximumBytes)} review limit.`}
      />
    );
  }

  const updates = uniqueApprovalCommits(review.result.updates);
  const addedCommitCount = updates.reduce(
    (count, update) => count + update.addedCommits.length,
    0,
  );

  return (
    <section className="border-y border-ui-border" aria-label="Commits added by this publication">
      <header className="flex items-center justify-between gap-3 bg-canvas/30 px-4 py-2.5">
        <h3 className="text-xs font-semibold text-muted">Added Commits</h3>
        <span className="font-mono text-[10px] text-subtle">{addedCommitCount}</span>
      </header>
      <div className={changes
        ? "grid min-h-0 lg:grid-cols-[minmax(15rem,20rem)_minmax(0,1fr)]"
        : ""}
      >
        <div className={changes
          ? "max-h-[28rem] overflow-y-auto lg:border-r lg:border-ui-border"
          : ""}
        >
          {updates.map((update) => (
            <ApprovalUpdateCommits
              key={update.reference}
              review={update}
              selected={changes?.selection}
              onSelect={loadCommitChanges}
            />
          ))}
          {addedCommitCount === 0 ? (
            <p className="px-4 py-4 text-xs leading-5 text-subtle">
              This publication changes references without introducing a new commit.
            </p>
          ) : null}
        </div>
        {changes ? <CommitChanges load={changes} /> : null}
      </div>
    </section>
  );
}

function ApprovalUpdateCommits({
  review,
  selected,
  onSelect,
}: {
  review: repositories.RepositoryApprovalUpdateReview;
  selected?: CommitSelection;
  onSelect: (selection: CommitSelection) => void;
}) {
  if (!review.addedCommits.length) return null;
  return (
    <section className="border-t border-ui-border first:border-t-0">
      <h4 className="flex flex-wrap items-center gap-x-2 bg-surface/40 px-4 py-2 text-[10px] text-subtle">
        <span>{approvalReferenceKind(review.reference)}</span>
        <span className="font-mono text-muted">{displayApprovalReference(review.reference)}</span>
      </h4>
      <ol className="list-none divide-y divide-ui-border p-0">
        {review.addedCommits.map((commit) => {
          const isSelected = selected?.reference === review.reference
            && selected.commit.id === commit.id;
          return (
            <li key={commit.id}>
              <button
                aria-pressed={isSelected}
                className="flex w-full min-w-0 items-start px-4 py-2.5 text-left outline-none hover:bg-surface-raised focus-visible:outline-2 focus-visible:outline-inset focus-visible:outline-accent data-[selected=true]:bg-accent/10"
                data-selected={isSelected}
                type="button"
                onClick={() => onSelect({ reference: review.reference, commit })}
              >
                <span className="min-w-0 flex-1">
                  <span className="block truncate text-xs text-foreground">
                    {commit.subject || "Untitled commit"}
                  </span>
                  <span className="mt-1 flex flex-wrap gap-x-2 text-[9px] text-subtle">
                    <span className="font-mono">{shortHash(commit.id)}</span>
                    <span>{commit.author.name}</span>
                    <span>{formatTimestamp(commit.author.timestamp)}</span>
                  </span>
                </span>
              </button>
            </li>
          );
        })}
      </ol>
    </section>
  );
}

function CommitChanges({ load }: { load: CommitChangesLoad }) {
  const heading = (
    <header className="border-b border-ui-border px-3 py-2.5">
      <h4 className="truncate text-xs font-medium text-foreground">
        {load.selection.commit.subject || "Untitled commit"}
      </h4>
      <p className="mt-1 font-mono text-[9px] text-subtle">
        {shortHash(load.selection.commit.id)}
      </p>
    </header>
  );
  if (load.status === "loading") {
    return (
      <div className="min-h-48 bg-canvas/30">
        {heading}
        <p className="flex items-center gap-2 p-4 text-xs text-subtle" aria-live="polite">
          <LoaderCircle aria-hidden="true" className="size-4 animate-spin" />
          Loading commit changes…
        </p>
      </div>
    );
  }
  if (load.status === "error") {
    return (
      <div className="min-h-48 bg-canvas/30">
        {heading}
        <ReviewError message={`Could not load commit changes: ${load.message}`} />
      </div>
    );
  }
  if (load.result.status === "TooLarge") {
    return (
      <div className="min-h-48 bg-canvas/30">
        {heading}
        <ReviewError
          message={`This commit's patch exceeds the ${formatBytes(load.result.maximumBytes)} review limit.`}
        />
      </div>
    );
  }
  return (
    <div className="min-h-48 min-w-0 bg-canvas/30">
      {heading}
      <div className="max-h-[24rem] min-w-0 overflow-auto">
        {String(load.result.diff) ? (
          <DiffViewer patch={String(load.result.diff)} />
        ) : (
          <p className="p-4 text-xs text-subtle">This commit has no text changes.</p>
        )}
      </div>
    </div>
  );
}

function ReviewError({ message }: { message: string }) {
  return (
    <p className="flex items-start gap-2 border-y border-red-500/20 bg-red-500/5 px-4 py-3 text-xs leading-5 text-red-200" role="alert">
      <AlertTriangle aria-hidden="true" className="mt-0.5 size-4 shrink-0" />
      {message}
    </p>
  );
}

function shortHash(value: changes.GitObjectId): string {
  return String(value).slice(0, 12);
}

function uniqueApprovalCommits(
  updates: repositories.RepositoryApprovalUpdateReview[],
): repositories.RepositoryApprovalUpdateReview[] {
  const seen = new Set<string>();
  return updates.map((update) => ({
    ...update,
    addedCommits: update.addedCommits.filter((commit) => {
      const id = String(commit.id);
      if (seen.has(id)) return false;
      seen.add(id);
      return true;
    }),
  }));
}

function formatTimestamp(value: string): string {
  const timestamp = new Date(value);
  return Number.isNaN(timestamp.getTime())
    ? value
    : timestamp.toLocaleString(undefined, {
        dateStyle: "medium",
        timeStyle: "short",
      });
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
