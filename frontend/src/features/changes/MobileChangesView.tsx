import {
  AlertTriangle,
  ArrowLeft,
  CheckCircle2,
  ChevronDown,
  FileDiff,
  LoaderCircle,
} from "lucide-react";
import { useEffect, useMemo, useState, type ReactNode } from "react";

import { guestApi } from "../../api/client.ts";
import type { changes, pods, shares, workspaces } from "../../api/generated/index.ts";
import { Badge } from "../../components/ui/Badge.tsx";
import { DiffViewer } from "../../components/ui/DiffViewer.tsx";
import { ChangedFileRow } from "./changePresentation.tsx";
import { MobileOverlayChangesReview } from "./MobileOverlayChangesReview.tsx";
import { useRepositoryStatuses, useShareOverlayApprovals } from "./state.ts";

type ChangeSetLoad =
  | { status: "loading" }
  | { status: "error"; message: string }
  | { status: "ready"; result: changes.ChangeSetResult };

type MobileChangeSource =
  | { type: "repository"; id: string; entry: changes.RepositoryStatusEntry }
  | { type: "overlay"; id: string; approval: shares.ShareOverlayApprovalRequest };

export function MobileChangesView({
  workspace,
  pod,
  review,
}: {
  workspace: workspaces.WorkspaceName;
  pod: pods.Pod;
  review?: {
    repository: string;
    base: changes.GitObjectId;
    head: changes.GitObjectId;
  };
}) {
  const statusState = useRepositoryStatuses(workspace);
  const overlayState = useShareOverlayApprovals(workspace);
  const repositories = useMemo(
    () => (statusState.value?.repositories ?? []).filter((entry) =>
      entry.target.podId === pod.id
        && entry.state.status === "Ready"
        && (
          entry.target.path === review?.repository
          ||
          entry.state.working.dirty
          || Number(entry.state.upstream?.ahead ?? 0) > 0
        )
    ),
    [pod.id, review?.repository, statusState.value?.repositories],
  );
  const overlays = useMemo(
    () => (overlayState.value?.requests ?? []).filter((request) => request.podId === pod.id),
    [overlayState.value?.requests, pod.id],
  );
  const sources = useMemo<MobileChangeSource[]>(() => [
    ...overlays.map((approval) => ({
      type: "overlay" as const,
      id: `overlay:${String(approval.id)}`,
      approval,
    })),
    ...repositories.map((entry) => ({
      type: "repository" as const,
      id: `repository:${String(entry.target.path)}`,
      entry,
    })),
  ], [overlays, repositories]);
  const [selectedSourceId, setSelectedSourceId] = useState<string>();
  const selectedSource = sources.find((source) => source.id === selectedSourceId) ?? sources[0];

  useEffect(() => {
    const reviewedSource = sources.find(
      (source) => source.type === "repository"
        && source.entry.target.path === review?.repository,
    );
    if (reviewedSource && selectedSourceId !== reviewedSource.id) {
      setSelectedSourceId(reviewedSource.id);
      return;
    }
    if (selectedSourceId && sources.some((source) => source.id === selectedSourceId)) return;
    setSelectedSourceId(reviewedSource?.id ?? sources[0]?.id);
  }, [review?.repository, selectedSourceId, sources]);

  if (!selectedSource && (statusState.error || overlayState.error)) {
    return <MobileChangeError message={statusState.error?.message ?? overlayState.error?.message ?? "Changes are unavailable."} />;
  }
  if (!selectedSource) {
    const ready = statusState.ready && overlayState.ready;
    return (
      <div className="flex h-full items-center justify-center p-6 text-center">
        <div>
          {ready ? (
            <CheckCircle2 aria-hidden="true" className="mx-auto size-8 text-emerald-400/60" />
          ) : (
            <LoaderCircle aria-hidden="true" className="mx-auto size-6 animate-spin text-accent-text" />
          )}
          <p className="mt-3 text-sm text-subtle">
            {ready ? "This pod has no changes awaiting review." : "Loading changes…"}
          </p>
        </div>
      </div>
    );
  }

  const sourcePicker = sources.length > 1 ? (
    <MobileChangeSourcePicker
      selectedSourceId={selectedSource.id}
      sources={sources}
      onSelect={setSelectedSourceId}
    />
  ) : undefined;
  const sourceNotice = statusState.error || overlayState.error ? (
    <MobileChangeError
      message={statusState.error?.message ?? overlayState.error?.message ?? "Some changes are unavailable."}
    />
  ) : undefined;

  return selectedSource.type === "repository" ? (
    <MobileRepositoryChanges
      entry={selectedSource.entry}
      review={review}
      sourceNotice={sourceNotice}
      sourcePicker={sourcePicker}
      workspace={workspace}
    />
  ) : (
    <MobileOverlayChangesReview
      approval={selectedSource.approval}
      sourceNotice={sourceNotice}
      sourcePicker={sourcePicker}
      workspace={workspace}
    />
  );
}

function MobileChangeSourcePicker({
  sources,
  selectedSourceId,
  onSelect,
}: {
  sources: readonly MobileChangeSource[];
  selectedSourceId: string;
  onSelect: (id: string) => void;
}) {
  return (
    <label className="relative block">
      <span className="sr-only">Change source</span>
      <FileDiff
        aria-hidden="true"
        className="pointer-events-none absolute left-3 top-1/2 z-10 size-4 -translate-y-1/2 text-subtle"
      />
      <select
        className="h-12 w-full min-w-0 max-w-full appearance-none rounded-xl border border-ui-border-strong bg-surface pl-10 pr-10 text-sm text-foreground outline-none focus:border-accent/50"
        value={selectedSourceId}
        onChange={(event) => onSelect(event.target.value)}
      >
        {sources.map((source) => (
          <option key={source.id} value={source.id}>
            {source.type === "repository"
              ? `/workspace/${source.entry.target.path}`
              : `/mnt/${source.approval.share} · Overlay`}
          </option>
        ))}
      </select>
      <ChevronDown
        aria-hidden="true"
        className="pointer-events-none absolute right-3 top-1/2 size-4 -translate-y-1/2 text-subtle"
      />
    </label>
  );
}

function MobileRepositoryChanges({
  entry,
  review,
  sourceNotice,
  sourcePicker,
  workspace,
}: {
  entry: changes.RepositoryStatusEntry;
  review?: {
    repository: string;
    base: changes.GitObjectId;
    head: changes.GitObjectId;
  };
  sourceNotice?: ReactNode;
  sourcePicker?: ReactNode;
  workspace: workspaces.WorkspaceName;
}) {
  const [overview, setOverview] = useState<ChangeSetLoad>({ status: "loading" });
  const [selectedFile, setSelectedFile] = useState<string>();
  const [fileDetail, setFileDetail] = useState<ChangeSetLoad>();
  const ready = entry.state.status === "Ready" ? entry.state : undefined;
  const comparison = useMemo(
    () => review && entry.target.path === review.repository
      ? {
          type: "Commits" as const,
          base: review.base,
          head: review.head,
        }
      : ready ? reviewComparison(ready) : undefined,
    [entry.target.path, ready, review],
  );

  useEffect(() => {
    setSelectedFile(undefined);
    setFileDetail(undefined);
    if (!comparison) return;
    const controller = new AbortController();
    setOverview({ status: "loading" });
    void guestApi(workspace).execute("changes_GetChangeSet", {
      target: entry.target,
      comparison,
    }, controller.signal).then(
      (output) => setOverview({ status: "ready", result: output.result }),
      (cause) => {
        if (!controller.signal.aborted) {
          setOverview({ status: "error", message: errorMessage(cause) });
        }
      },
    );
    return () => controller.abort();
  }, [comparison, entry, workspace]);

  useEffect(() => {
    if (!selectedFile) {
      setFileDetail(undefined);
      return;
    }
    const controller = new AbortController();
    setFileDetail({ status: "loading" });
    void guestApi(workspace).execute("changes_GetChangeSet", {
      target: entry.target,
      comparison: comparison ?? { type: "Working" },
      path: selectedFile as changes.RepositoryPath,
    }, controller.signal).then(
      (output) => setFileDetail({ status: "ready", result: output.result }),
      (cause) => {
        if (!controller.signal.aborted) {
          setFileDetail({ status: "error", message: errorMessage(cause) });
        }
      },
    );
    return () => controller.abort();
  }, [comparison, entry.target, selectedFile, workspace]);

  const overviewSet = changeSet(overview);
  if (selectedFile) {
    return (
      <div className="flex h-full min-h-0 flex-col overflow-hidden bg-canvas">
        <header className="mobile-client-horizontal flex min-h-14 items-center gap-2 border-b border-ui-border">
          <button
            className="flex size-11 shrink-0 items-center justify-center rounded-xl text-muted active:bg-surface-raised"
            type="button"
            aria-label="Changed file list"
            onClick={() => setSelectedFile(undefined)}
          >
            <ArrowLeft aria-hidden="true" className="size-5" />
          </button>
          <div className="min-w-0 flex-1">
            <h2 className="truncate font-mono text-xs font-semibold text-foreground">
              {selectedFile}
            </h2>
            <p className="mt-0.5 truncate text-[10px] text-subtle">
              /workspace/{entry.target.path}
            </p>
          </div>
        </header>
        <div className="min-h-0 flex-1 overflow-auto">
          <MobileFileDiff load={fileDetail} fileName={selectedFile} />
        </div>
      </div>
    );
  }

  return (
    <div className="mobile-client-content h-full min-h-0 overflow-y-auto pt-4">
      <div className="mx-auto w-full min-w-0 max-w-2xl">
        {sourcePicker ?? (
          <div className="rounded-xl border border-ui-border bg-surface/70 p-3">
            <p className="truncate font-mono text-xs font-semibold text-foreground">
              /workspace/{entry.target.path}
            </p>
          </div>
        )}
        {sourceNotice}

        {ready ? (
          <div className="mt-3 flex flex-wrap items-center gap-2">
            <Badge tone="warning">
              {review && entry.target.path === review.repository
                ? "Proposed publication"
                : ready.working.dirty
                ? `${String(overviewSet?.summary.fileCount ?? ready.working.fileCount)} changed`
                : `${String(ready.upstream?.ahead ?? 0)} unpushed commits`}
            </Badge>
            {Number(ready.working.conflictCount) > 0 ? (
              <Badge tone="danger">{String(ready.working.conflictCount)} conflicts</Badge>
            ) : null}
            <span className="text-[11px] text-subtle">{ready.branch ?? "Detached HEAD"}</span>
          </div>
        ) : null}

        <div className="mt-5 grid gap-2">
          {overview.status === "loading" ? (
            <p className="flex items-center gap-2 p-4 text-sm text-subtle">
              <LoaderCircle aria-hidden="true" className="size-4 animate-spin" />
              Loading changed files…
            </p>
          ) : overview.status === "error" ? (
            <MobileChangeError message={overview.message} />
          ) : overview.result.status !== "ChangeSet" ? (
            <MobileChangeError message={changeSetResultMessage(overview.result)} />
          ) : (
            overviewSet?.files.map((file) => {
              const path = displayPath(file);
              return (
                <ChangedFileRow
                  kind={file.kind.tag}
                  key={`${file.kind.tag}:${file.oldPath ?? ""}:${file.newPath ?? ""}`}
                  metadata={file.binary ? (
                    <Badge size="xs">Binary</Badge>
                  ) : (
                    <span className="shrink-0 font-mono text-[10px]">
                      <span className="text-emerald-300">+{String(file.lines.additions)}</span>{" "}
                      <span className="text-red-300">−{String(file.lines.deletions)}</span>
                    </span>
                  )}
                  mobile
                  path={path}
                  onSelect={() => setSelectedFile(path)}
                />
              );
            })
          )}
        </div>
      </div>
    </div>
  );
}

function MobileFileDiff({
  load,
  fileName,
}: {
  load?: ChangeSetLoad;
  fileName: string;
}) {
  if (!load || load.status === "loading") {
    return (
      <p className="flex items-center gap-2 p-4 text-sm text-subtle">
        <LoaderCircle aria-hidden="true" className="size-4 animate-spin" />
        Loading diff…
      </p>
    );
  }
  if (load.status === "error") return <MobileChangeError message={load.message} />;
  if (load.result.status !== "ChangeSet") {
    return <MobileChangeError message={changeSetResultMessage(load.result)} />;
  }
  if (!String(load.result.diff)) {
    return (
      <p className="p-5 text-sm leading-6 text-subtle">
        {load.result.files.some((file) => file.binary)
          ? "This is a binary change; no text diff is available."
          : "No text changes are available."}
      </p>
    );
  }
  return <DiffViewer patch={String(load.result.diff)} fileName={fileName} />;
}

function MobileChangeError({ message }: { message: string }) {
  return (
    <div className="p-4">
      <p className="flex items-start gap-2 rounded-xl border border-red-500/20 bg-red-500/5 p-3 text-xs leading-5 text-red-200" role="alert">
        <AlertTriangle aria-hidden="true" className="mt-0.5 size-4 shrink-0" />
        {message}
      </p>
    </div>
  );
}

function changeSet(load: ChangeSetLoad): changes.FileChangeSet | undefined {
  return load.status === "ready" && load.result.status === "ChangeSet"
    ? load.result
    : undefined;
}

function displayPath(file: changes.FileChange): string {
  return String(file.newPath ?? file.oldPath ?? "");
}

function changeSetResultMessage(result: changes.ChangeSetResult): string {
  if (result.status === "TooLarge") {
    return `This change set exceeds the ${formatBytes(result.maximumBytes)} control-plane limit.`;
  }
  if (result.status === "RevisionUnavailable") {
    return `Git revision ${String(result.revision).slice(0, 8)} is no longer available.`;
  }
  if (result.status === "UnrelatedHistories") {
    return "The selected revisions do not have a common ancestor.";
  }
  return "No changed files are available.";
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

function reviewComparison(
  repository: Extract<changes.RepositoryStatusState, { status: "Ready" }>,
): changes.ChangeSetComparison | undefined {
  if (repository.working.dirty) return { type: "Working" };
  if (repository.head && repository.upstream && Number(repository.upstream.ahead) > 0) {
    return {
      type: "Commits",
      base: repository.upstream.commit.id,
      head: repository.head.id,
    };
  }
  return undefined;
}
