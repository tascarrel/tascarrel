import {
  AlertTriangle,
  CheckCircle2,
  ChevronRight,
  FileDiff,
  GitBranch,
  GitCommitHorizontal,
  LoaderCircle,
} from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";

import { guestApi } from "../../api/client.ts";
import type { changes, pods, workspaces } from "../../api/generated/index.ts";
import { Badge } from "../../components/ui/Badge.tsx";
import { Button } from "../../components/ui/Button.tsx";
import { DiffViewer } from "../../components/ui/DiffViewer.tsx";
import { useRepositoryStatuses } from "./state.ts";

type ChangeSetLoad =
  | { status: "loading" }
  | { status: "error"; message: string }
  | { status: "ready"; result: changes.ChangeSetResult };

type DivergenceLoad =
  | { status: "loading" }
  | { status: "error"; message: string }
  | { status: "ready"; result: changes.DivergentCommitsResult };

export function ChangesView({
  workspace,
  pod,
}: {
  workspace: workspaces.WorkspaceName;
  pod: pods.Pod;
}) {
  const statusState = useRepositoryStatuses(workspace);
  const repositories = useMemo(
    () => (statusState.value?.repositories ?? []).filter((entry) =>
      entry.target.podId === pod.id
        && entry.state.status === "Ready"
        && entry.state.working.dirty
    ),
    [pod.id, statusState.value?.repositories],
  );
  const [selectedPath, setSelectedPath] = useState<string>();
  const selected = repositories.find((entry) => entry.target.path === selectedPath)
    ?? repositories[0];

  useEffect(() => {
    if (selectedPath && repositories.some((entry) => entry.target.path === selectedPath)) return;
    setSelectedPath(repositories[0]?.target.path);
  }, [repositories, selectedPath]);

  return (
    <div className="grid h-full min-h-0 grid-cols-[minmax(15rem,20rem)_minmax(0,1fr)] overflow-hidden bg-canvas text-foreground">
      <section className="flex min-h-0 flex-col border-r border-ui-border" aria-label="Pod repositories">
        {statusState.error ? (
          <p className="border-b border-red-500/20 bg-red-500/5 px-3 py-2 text-[10px] text-red-200" role="alert">
            {statusState.error.message}
          </p>
        ) : null}
        <div className="min-h-0 flex-1 overflow-auto p-2">
          {repositories.map((entry) => (
            <RepositoryRow
              entry={entry}
              key={String(entry.target.path)}
              selected={entry.target.path === selected?.target.path}
              onSelect={() => setSelectedPath(entry.target.path)}
            />
          ))}
          {!repositories.length ? (
            <div className="rounded-lg border border-dashed border-ui-border p-4 text-center text-[11px] leading-5 text-subtle">
              {statusState.ready ? "No repositories have changes." : "Loading repository changes…"}
            </div>
          ) : null}
        </div>
      </section>

      {selected ? (
        <RepositoryChanges workspace={workspace} entry={selected} />
      ) : (
        <div className="flex min-h-0 items-center justify-center p-8 text-center text-xs text-subtle">
          {statusState.ready ? "No repository changes to inspect." : "Loading repository changes…"}
        </div>
      )}
    </div>
  );
}

function RepositoryRow({
  entry,
  selected,
  onSelect,
}: {
  entry: changes.RepositoryStatusEntry;
  selected: boolean;
  onSelect: () => void;
}) {
  const ready = entry.state.status === "Ready" ? entry.state : undefined;
  return (
    <button
      aria-pressed={selected}
      className="mb-1 flex w-full min-w-0 items-center gap-2 rounded-lg border border-transparent px-2.5 py-2 text-left outline-none transition hover:border-ui-border hover:bg-surface focus-visible:outline-2 focus-visible:outline-accent data-[selected=true]:border-ui-border-strong data-[selected=true]:bg-surface-raised"
      data-selected={selected}
      type="button"
      onClick={onSelect}
    >
      <GitBranch aria-hidden="true" className="size-3.5 shrink-0 text-subtle" />
      <span className="min-w-0 flex-1">
        <span className="block truncate font-mono text-[11px] text-foreground">
          /workspace/{entry.target.path}
        </span>
        <span className="mt-0.5 block truncate text-[10px] text-subtle">
          {ready ? ready.branch ?? "Detached HEAD" : "Inspection failed"}
        </span>
      </span>
      {ready?.working.dirty ? (
        <Badge size="xs" tone={positiveCount(ready.working.conflictCount) ? "danger" : "warning"}>
          {String(ready.working.fileCount)}
        </Badge>
      ) : ready ? (
        <CheckCircle2 aria-label="Clean" className="size-3.5 shrink-0 text-emerald-400/70" />
      ) : (
        <AlertTriangle aria-label="Failed" className="size-3.5 shrink-0 text-red-300" />
      )}
      <ChevronRight aria-hidden="true" className="size-3 shrink-0 text-subtle" />
    </button>
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
                <button
                  aria-pressed={selectedFile === path}
                  className="mb-0.5 flex w-full min-w-0 items-center gap-2 rounded-lg px-2.5 py-1.5 text-left text-[11px] text-muted hover:bg-surface data-[selected=true]:bg-accent/10 data-[selected=true]:text-accent-text"
                  data-selected={selectedFile === path}
                  title={path}
                  type="button"
                  key={`${file.kind.tag}:${file.oldPath ?? ""}:${file.newPath ?? ""}`}
                  onClick={() => setSelectedFile(path)}
                >
                  <ChangeKind kind={file.kind} />
                  <span className="min-w-0 flex-1 truncate">{path}</span>
                  {file.binary ? <span className="text-[9px] text-subtle">BIN</span> : (
                    <span className="shrink-0 font-mono text-[9px]">
                      <span className="text-emerald-300">+{String(file.lines.additions)}</span>{" "}
                      <span className="text-red-300">−{String(file.lines.deletions)}</span>
                    </span>
                  )}
                </button>
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

function ChangeKind({ kind }: { kind: changes.FileChange["kind"] }) {
  const label = kind.tag === "Untracked" ? "U" : kind.tag.slice(0, 1);
  const tone = kind.tag === "Deleted" || kind.tag === "Unmerged"
    ? "text-red-300"
    : kind.tag === "Added" || kind.tag === "Untracked"
      ? "text-emerald-300"
      : "text-amber-300";
  return <span aria-label={kind.tag} className={`w-3 shrink-0 font-mono text-[10px] ${tone}`} title={kind.tag}>{label}</span>;
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
