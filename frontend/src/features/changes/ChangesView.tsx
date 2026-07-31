import {
  CheckCircle2,
  FileDiff,
  GitBranch,
  GitCommitHorizontal,
  Layers3,
  LoaderCircle,
} from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";

import { guestApi } from "../../api/client.ts";
import type { changes, pods, shares, workspaces } from "../../api/generated/index.ts";
import { Badge } from "../../components/ui/Badge.tsx";
import { Button } from "../../components/ui/Button.tsx";
import { DiffViewer } from "../../components/ui/DiffViewer.tsx";
import { ChangedFileRow, ChangeSourceRow } from "./changePresentation.tsx";
import { OverlayChangesReview } from "./OverlayChangesReview.tsx";
import { useRepositoryStatuses, useShareOverlayApprovals } from "./state.ts";

type ChangeSetLoad =
  | { status: "loading" }
  | { status: "error"; message: string }
  | { status: "ready"; result: changes.ChangeSetResult };

type DivergenceLoad =
  | { status: "loading" }
  | { status: "error"; message: string }
  | { status: "ready"; result: changes.DivergentCommitsResult };

type ChangeSource =
  | { type: "repository"; id: string; entry: changes.RepositoryStatusEntry }
  | { type: "overlay"; id: string; approval: shares.ShareOverlayApprovalRequest };

export function ChangesView({
  workspace,
  pod,
}: {
  workspace: workspaces.WorkspaceName;
  pod: pods.Pod;
}) {
  const statusState = useRepositoryStatuses(workspace);
  const overlayState = useShareOverlayApprovals(workspace);
  const repositories = useMemo(
    () => (statusState.value?.repositories ?? []).filter((entry) =>
      entry.target.podId === pod.id
        && entry.state.status === "Ready"
        && entry.state.working.dirty
    ),
    [pod.id, statusState.value?.repositories],
  );
  const overlays = useMemo(
    () => (overlayState.value?.requests ?? []).filter((request) => request.podId === pod.id),
    [overlayState.value?.requests, pod.id],
  );
  const sources = useMemo<ChangeSource[]>(() => [
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
  const selected = sources.find((source) => source.id === selectedSourceId) ?? sources[0];

  useEffect(() => {
    if (selectedSourceId && sources.some((source) => source.id === selectedSourceId)) return;
    setSelectedSourceId(sources[0]?.id);
  }, [selectedSourceId, sources]);

  const ready = statusState.ready && overlayState.ready;

  return (
    <div className="grid h-full min-h-0 grid-cols-[minmax(15rem,20rem)_minmax(0,1fr)] overflow-hidden bg-canvas text-foreground">
      <section className="flex min-h-0 flex-col border-r border-ui-border" aria-label="Pod change sources">
        {statusState.error || overlayState.error ? (
          <p className="border-b border-red-500/20 bg-red-500/5 px-3 py-2 text-[10px] text-red-200" role="alert">
            {statusState.error?.message ?? overlayState.error?.message}
          </p>
        ) : null}
        <div className="min-h-0 flex-1 overflow-auto p-2">
          {sources.map((source) => source.type === "repository" ? (
            <ChangeSourceRow
              count={source.entry.state.status === "Ready"
                ? Number(source.entry.state.working.fileCount)
                : 0}
              countTone={source.entry.state.status === "Ready"
                  && positiveCount(source.entry.state.working.conflictCount)
                ? "danger"
                : "warning"}
              icon={<GitBranch aria-hidden="true" className="size-3.5" />}
              key={source.id}
              selected={source.id === selected?.id}
              subtitle={source.entry.state.status === "Ready"
                ? source.entry.state.branch ?? "Detached HEAD"
                : "Inspection failed"}
              title={`/workspace/${source.entry.target.path}`}
              onSelect={() => setSelectedSourceId(source.id)}
            />
          ) : (
            <ChangeSourceRow
              count={source.approval.changes.length}
              icon={<Layers3 aria-hidden="true" className="size-3.5" />}
              key={source.id}
              selected={source.id === selected?.id}
              subtitle="Overlay share · awaiting approval"
              title={`/mnt/${source.approval.share}`}
              onSelect={() => setSelectedSourceId(source.id)}
            />
          ))}
          {!sources.length ? (
            <div className="rounded-lg border border-dashed border-ui-border p-4 text-center text-[11px] leading-5 text-subtle">
              {ready ? "This pod has no changes awaiting review." : "Loading changes…"}
            </div>
          ) : null}
        </div>
      </section>

      {selected?.type === "repository" ? (
        <RepositoryChanges workspace={workspace} entry={selected.entry} />
      ) : selected?.type === "overlay" ? (
        <OverlayChangesReview workspace={workspace} approval={selected.approval} />
      ) : (
        <div className="flex min-h-0 items-center justify-center p-8 text-center text-xs text-subtle">
          {ready ? "No changes to inspect." : "Loading changes…"}
        </div>
      )}
    </div>
  );
}

function RepositoryChanges({
  workspace,
  entry,
}: {
  workspace: workspaces.WorkspaceName;
  entry: changes.RepositoryStatusEntry;
}) {
  const [overview, setOverview] = useState<ChangeSetLoad>();
  const [selectedFile, setSelectedFile] = useState<string>();
  const [fileDetail, setFileDetail] = useState<ChangeSetLoad>();
  const [divergence, setDivergence] = useState<DivergenceLoad>();
  const divergenceRequest = useRef<AbortController | undefined>(undefined);
  const ready = entry.state.status === "Ready" ? entry.state : undefined;

  useEffect(() => () => divergenceRequest.current?.abort(), []);

  useEffect(() => {
    setSelectedFile(undefined);
    setFileDetail(undefined);
    setDivergence(undefined);
    divergenceRequest.current?.abort();
    if (!ready?.working.dirty) {
      setOverview(undefined);
      return;
    }
    const controller = new AbortController();
    setOverview({ status: "loading" });
    void guestApi(workspace).execute("changes_GetChangeSet", {
      target: entry.target,
      comparison: { type: "Working" },
    }, controller.signal).then(
      (output) => setOverview({ status: "ready", result: output.result }),
      (cause) => {
        if (!controller.signal.aborted) setOverview({ status: "error", message: errorMessage(cause) });
      },
    );
    return () => controller.abort();
  }, [entry, ready, workspace]);

  useEffect(() => {
    if (!selectedFile) {
      setFileDetail(undefined);
      return;
    }
    const controller = new AbortController();
    setFileDetail({ status: "loading" });
    void guestApi(workspace).execute("changes_GetChangeSet", {
      target: entry.target,
      comparison: { type: "Working" },
      path: selectedFile as changes.RepositoryPath,
    }, controller.signal).then(
      (output) => setFileDetail({ status: "ready", result: output.result }),
      (cause) => {
        if (!controller.signal.aborted) setFileDetail({ status: "error", message: errorMessage(cause) });
      },
    );
    return () => controller.abort();
  }, [entry, selectedFile, workspace]);

  const loadDivergence = async () => {
    if (!ready?.head || !ready.upstream || divergence?.status === "loading") return;
    divergenceRequest.current?.abort();
    const controller = new AbortController();
    divergenceRequest.current = controller;
    setDivergence({ status: "loading" });
    try {
      const output = await guestApi(workspace).execute("changes_GetDivergentCommits", {
        target: entry.target,
        comparison: { head: ready.head.id, upstream: ready.upstream.commit.id },
      }, controller.signal);
      setDivergence({ status: "ready", result: output.result });
    } catch (cause) {
      if (!controller.signal.aborted) {
        setDivergence({ status: "error", message: errorMessage(cause) });
      }
    } finally {
      if (divergenceRequest.current === controller) divergenceRequest.current = undefined;
    }
  };

  if (!ready) {
    const message = entry.state.status === "Failed"
      ? entry.state.message
      : "Repository status is unavailable.";
    return (
      <div className="flex min-h-0 items-center justify-center p-8">
        <p className="max-w-lg rounded-lg border border-red-500/20 bg-red-500/5 p-4 text-xs leading-5 text-red-200" role="alert">
          {message}
        </p>
      </div>
    );
  }

  const overviewSet = changeSet(overview);
  const displayed = selectedFile ? fileDetail : overview;
  const displayedSet = changeSet(displayed);

  return (
    <section className="flex min-h-0 flex-col overflow-hidden" aria-label={`Changes in ${entry.target.path}`}>
      <header className="border-b border-ui-border px-4 py-3">
        <div className="flex flex-wrap items-start justify-between gap-3">
          <div className="min-w-0">
            <h2 className="truncate font-mono text-xs font-semibold">/workspace/{entry.target.path}</h2>
            <p className="mt-1 flex flex-wrap items-center gap-x-2 gap-y-1 text-[10px] text-subtle">
              <span>{ready.branch ?? "Detached HEAD"}</span>
              {ready.head ? <span title={ready.head.id}>{shortHash(ready.head.id)} · {ready.head.subject || "Untitled commit"}</span> : <span>Unborn repository</span>}
            </p>
          </div>
          <div className="flex flex-wrap items-center gap-1.5">
            <Badge size="xs" tone={ready.working.dirty ? "warning" : "success"}>
              {ready.working.dirty ? `${String(ready.working.fileCount)} changed` : "Clean"}
            </Badge>
            {positiveCount(ready.working.conflictCount) ? <Badge size="xs" tone="danger">{String(ready.working.conflictCount)} conflicts</Badge> : null}
            {ready.upstream ? (
              <Badge size="xs" tone={positiveCount(ready.upstream.ahead) || positiveCount(ready.upstream.behind) ? "primary" : "muted"}>
                ↑{String(ready.upstream.ahead)} ↓{String(ready.upstream.behind)}
              </Badge>
            ) : <Badge size="xs" tone="muted">No upstream</Badge>}
          </div>
        </div>
        {ready.upstream && (positiveCount(ready.upstream.ahead) || positiveCount(ready.upstream.behind)) ? (
          <div className="mt-3">
            <Button size="small" disabled={divergence?.status === "loading"} onClick={() => void loadDivergence()}>
              {divergence?.status === "loading"
                ? <LoaderCircle aria-hidden="true" className="size-3.5 animate-spin" />
                : <GitCommitHorizontal aria-hidden="true" className="size-3.5" />}
              {divergence ? "Reload commits" : "Show ahead/behind commits"}
            </Button>
          </div>
        ) : null}
        {divergence ? <DivergenceResult load={divergence} /> : null}
      </header>

      {!ready.working.dirty ? (
        <div className="flex min-h-0 flex-1 flex-col items-center justify-center gap-3 p-8 text-center text-xs text-subtle">
          <CheckCircle2 aria-hidden="true" className="size-8 text-emerald-400/60" />
          The index and working tree are clean.
        </div>
      ) : (
        <div className="grid min-h-0 flex-1 grid-cols-[minmax(13rem,18rem)_minmax(0,1fr)] overflow-hidden">
          <aside className="min-h-0 overflow-auto border-r border-ui-border p-2" aria-label="Changed files">
            <button
              aria-pressed={!selectedFile}
              className="mb-1 flex w-full items-center gap-2 rounded-lg px-2.5 py-2 text-left text-[11px] text-muted hover:bg-surface data-[selected=true]:bg-surface-raised data-[selected=true]:text-foreground"
              data-selected={!selectedFile}
              type="button"
              onClick={() => setSelectedFile(undefined)}
            >
              <FileDiff aria-hidden="true" className="size-3.5" /> All changes
              {overviewSet ? <span className="ml-auto font-mono text-[10px] text-subtle">{String(overviewSet.summary.fileCount)}</span> : null}
            </button>
            {overviewSet?.files.map((file) => {
              const path = displayPath(file);
              return (
                <ChangedFileRow
                  kind={file.kind.tag}
                  key={`${file.kind.tag}:${file.oldPath ?? ""}:${file.newPath ?? ""}`}
                  metadata={file.binary ? <span className="text-[9px] text-subtle">BIN</span> : (
                    <span className="shrink-0 font-mono text-[9px]">
                      <span className="text-emerald-300">+{String(file.lines.additions)}</span>{" "}
                      <span className="text-red-300">−{String(file.lines.deletions)}</span>
                    </span>
                  )}
                  path={path}
                  selected={selectedFile === path}
                  onSelect={() => setSelectedFile(path)}
                />
              );
            })}
          </aside>
          <div className="min-h-0 overflow-auto">
            <ChangeSetResult load={displayed} set={displayedSet} selectedFile={selectedFile} />
          </div>
        </div>
      )}
    </section>
  );
}

function ChangeSetResult({
  load,
  set,
  selectedFile,
}: {
  load?: ChangeSetLoad;
  set?: changes.FileChangeSet;
  selectedFile?: string;
}) {
  if (!load || load.status === "loading") {
    return <p className="flex items-center gap-2 text-xs text-subtle"><LoaderCircle aria-hidden="true" className="size-3.5 animate-spin" /> Loading changes…</p>;
  }
  if (load.status === "error") return <ErrorMessage message={load.message} />;
  if (load.result.status === "TooLarge") {
    return <ErrorMessage message={`This change set exceeds the ${formatBytes(load.result.maximumBytes)} control-plane limit.`} />;
  }
  if (load.result.status === "RevisionUnavailable") {
    return <ErrorMessage message={`Git revision ${shortHash(load.result.revision)} is no longer available.`} />;
  }
  if (load.result.status === "UnrelatedHistories") {
    return <ErrorMessage message="The selected commits do not have a common ancestor." />;
  }
  if (!set || !String(set.diff)) {
    return <p className="text-xs text-subtle">{set?.files.some((file) => file.binary) ? "Binary change; no text diff is available." : "No text changes."}</p>;
  }
  return <DiffViewer patch={String(set.diff)} fileName={selectedFile} />;
}

function DivergenceResult({ load }: { load: DivergenceLoad }) {
  if (load.status === "loading") return null;
  if (load.status === "error") return <ErrorMessage message={load.message} compact />;
  if (load.result.status === "TooLarge") return <ErrorMessage message={`Commit details exceed ${formatBytes(load.result.maximumBytes)}.`} compact />;
  if (load.result.status === "RevisionUnavailable") return <ErrorMessage message={`Revision ${shortHash(load.result.revision)} is no longer available.`} compact />;
  return (
    <div className="mt-3 grid gap-3 text-[10px] sm:grid-cols-2">
      <CommitList title={`Ahead (${load.result.ahead.length})`} commits={load.result.ahead} />
      <CommitList title={`Behind (${load.result.behind.length})`} commits={load.result.behind} />
    </div>
  );
}

function CommitList({ title, commits }: { title: string; commits: readonly changes.GitCommit[] }) {
  return (
    <div className="rounded-lg border border-ui-border bg-surface/50 p-2">
      <h3 className="font-medium text-muted">{title}</h3>
      {commits.length ? (
        <ol className="mt-1.5 list-none space-y-1 p-0">
          {commits.map((commit) => (
            <li className="flex min-w-0 gap-2" key={commit.id} title={commit.id}>
              <span className="shrink-0 font-mono text-accent-text">{shortHash(commit.id)}</span>
              <span className="truncate text-subtle">{commit.subject || "Untitled commit"}</span>
            </li>
          ))}
        </ol>
      ) : <p className="mt-1 text-subtle">None</p>}
    </div>
  );
}

function ErrorMessage({ message, compact = false }: { message: string; compact?: boolean }) {
  return (
    <p className={`${compact ? "mt-3" : ""} rounded-lg border border-red-500/20 bg-red-500/5 px-3 py-2 text-xs leading-5 text-red-200`} role="alert">
      {message}
    </p>
  );
}

function changeSet(load?: ChangeSetLoad): changes.FileChangeSet | undefined {
  return load?.status === "ready" && load.result.status === "ChangeSet" ? load.result : undefined;
}

function displayPath(file: changes.FileChange): string {
  return String(file.newPath ?? file.oldPath ?? "");
}

function shortHash(id: changes.GitObjectId): string {
  return String(id).slice(0, 8);
}

function formatBytes(value: string | number): string {
  const bytes = Number(value);
  if (bytes >= 1024 * 1024) return `${(bytes / (1024 * 1024)).toFixed(1)} MiB`;
  if (bytes >= 1024) return `${(bytes / 1024).toFixed(1)} KiB`;
  return `${String(bytes)} B`;
}

function positiveCount(value: string | number): boolean {
  return Number(value) > 0;
}

function errorMessage(cause: unknown): string {
  return cause instanceof Error ? cause.message : String(cause);
}
