import {
  AlertTriangle,
  Boxes,
  Check,
  Database,
  GitBranch,
  LoaderCircle,
  RefreshCw,
  ShieldCheck,
  Tag,
  X,
} from "lucide-react";
import { useEffect, useState, type ReactNode } from "react";

import { hostApi } from "../../api/client.ts";
import type { repositories, workspaces } from "../../api/generated/index.ts";
import { Badge, type BadgeTone } from "../../components/ui/Badge.tsx";
import { Button } from "../../components/ui/Button.tsx";
import { useRepositories, useRepositoryApprovals } from "./state.ts";

export function RepositoriesView({ workspace }: { workspace: workspaces.WorkspaceName }) {
  const state = useRepositories(workspace);
  const approvalState = useRepositoryApprovals(workspace);
  const inventory = state.value?.repositories ?? [];
  const approvals = approvalState.value?.requests ?? [];
  const [actionError, setActionError] = useState<string>();
  const [refreshingCache, setRefreshingCache] = useState<string>();
  const reconnecting = [state.connection, approvalState.connection]
    .some((connection) => connection === "connecting" || connection === "reconnecting");

  useEffect(() => {
    setRefreshingCache(undefined);
    setActionError(undefined);
  }, [workspace]);

  const refreshCaches = async (path?: string) => {
    if (refreshingCache) return;
    setRefreshingCache(path ?? "*");
    setActionError(undefined);
    try {
      await hostApi.execute("repositories_RefreshSnapshot", {
        workspace,
        ...(path === undefined ? {} : { path }),
      });
      state.refresh();
    } catch (cause) {
      setActionError(errorMessage(cause));
    } finally {
      setRefreshingCache(undefined);
    }
  };

  const resolveApproval = async (
    approval: repositories.RepositoryApprovalRequest,
    decision: repositories.RepositoryApprovalDecision,
  ) => {
    setActionError(undefined);
    try {
      await hostApi.execute("repositories_ResolveApproval", {
        workspace,
        approvalId: approval.id,
        decision,
      });
    } catch (cause) {
      setActionError(errorMessage(cause));
    }
  };

  const visibleError = actionError ?? approvalState.error?.message ?? state.error?.message;

  return (
    <div className="flex h-full min-h-0 flex-col overflow-hidden bg-canvas text-foreground">
      <header className="flex flex-wrap items-center justify-between gap-3 border-b border-ui-border px-5 py-4">
        <div>
          <h1 className="flex items-center gap-2 text-sm font-semibold">
            <GitBranch aria-hidden="true" className="size-4 text-accent-text" /> Repositories
          </h1>
          <p className="mt-1 text-xs text-subtle">
            Inspect repository declarations and host cache state for {workspace}.
          </p>
        </div>
        <Button
          size="small"
          disabled={reconnecting || refreshingCache !== undefined}
          onClick={() => void refreshCaches()}
        >
          {refreshingCache === "*" ? (
            <LoaderCircle aria-hidden="true" className="size-3.5 animate-spin" />
          ) : (
            <RefreshCw aria-hidden="true" className="size-3.5" />
          )}
          {refreshingCache === "*" ? "Refreshing…" : "Refresh all"}
        </Button>
      </header>

      {visibleError ? (
        <p className="border-b border-red-500/20 bg-red-500/5 px-5 py-2.5 text-xs text-red-200" role="alert">
          {visibleError}
        </p>
      ) : null}

      <div className="min-h-0 flex-1 overflow-y-auto p-4 sm:p-6">
        <div className="mx-auto grid max-w-6xl gap-6">
          <section aria-labelledby="repository-approvals-title">
            <div className="mb-3 flex items-center justify-between gap-3">
              <div>
                <h2 className="flex items-center gap-1.5 text-xs font-semibold text-muted" id="repository-approvals-title">
                  <ShieldCheck aria-hidden="true" className="size-3.5 text-accent-text" /> Publication Approvals
                </h2>
                <p className="mt-1 text-[11px] text-subtle">
                  Pushes requiring approval stay in the workspace cache until you approve their exact ref updates.
                </p>
              </div>
              <Badge size="xs" tone={approvals.length ? "warning" : "muted"}>
                {approvals.length} unresolved
              </Badge>
            </div>

            {approvals.length ? (
              <ul className="grid list-none gap-3 p-0">
                {approvals.map((approval) => (
                  <ApprovalCard
                    approval={approval}
                    key={approval.id}
                    onApprove={() => void resolveApproval(approval, { tag: "Approve" })}
                    onReject={() => void resolveApproval(approval, { tag: "Reject" })}
                  />
                ))}
              </ul>
            ) : (
              <div className="rounded-xl border border-dashed border-ui-border p-6 text-center text-xs leading-5 text-subtle">
                {approvalState.ready
                  ? "No repository publications are waiting for approval."
                  : "Loading repository approval requests…"}
              </div>
            )}
          </section>

          <section aria-labelledby="repository-inventory-title">
            <div className="mb-3 flex items-center justify-between gap-3">
              <div>
                <h2 className="text-xs font-semibold text-muted" id="repository-inventory-title">
                  Configured Repositories
                </h2>
                <p className="mt-1 text-[11px] text-subtle">
                  Cache statistics are local to hostd and never contact the upstream.
                </p>
              </div>
              <span className="font-mono text-[11px] text-subtle">{inventory.length}</span>
            </div>

            {inventory.length ? (
              <ul className="grid list-none gap-3 p-0 lg:grid-cols-2">
                {inventory.map((repository) => (
                  <RepositoryCard
                    repository={repository}
                    key={repository.path}
                    disabled={refreshingCache !== undefined}
                    refreshing={refreshingCache === repository.path}
                    onRefresh={() => void refreshCaches(repository.path)}
                  />
                ))}
              </ul>
            ) : (
              <div className="rounded-xl border border-dashed border-ui-border p-8 text-center text-xs leading-5 text-subtle">
                {state.ready
                  ? "No repositories are declared in this workspace's config.toml."
                  : "Loading repository inventory…"}
              </div>
            )}
          </section>
        </div>
      </div>
    </div>
  );
}

function ApprovalCard({
  approval,
  onApprove,
  onReject,
}: {
  approval: repositories.RepositoryApprovalRequest;
  onApprove: () => void;
  onReject: () => void;
}) {
  const publishing = approval.status.tag === "Publishing";
  const postponed = approval.status.tag === "Pending" && approval.postponed;
  return (
    <li className="overflow-hidden rounded-xl border border-amber-500/20 bg-surface/60">
      <div className="flex flex-wrap items-start justify-between gap-3 border-b border-ui-border px-4 py-3">
        <div className="min-w-0">
          <h3 className="font-mono text-xs font-semibold text-foreground">
            /workspace/{approval.path}
          </h3>
          <p className="mt-1 break-all font-mono text-[10px] leading-4 text-subtle">
            {approval.source}
          </p>
          <p className="mt-1 text-[10px] text-subtle">
            Pod {shortId(approval.podId)} requested {formatTimestamp(approval.createdAt)}
          </p>
        </div>
        <Badge size="xs" tone="warning">
          {publishing ? "Publishing" : postponed ? "Postponed" : "Review required"}
        </Badge>
      </div>

      {approval.status.tag === "Failed" ? (
        <p className="border-b border-red-500/20 bg-red-500/5 px-4 py-2 text-[11px] text-red-200" role="alert">
          Publication failed: {approval.status.content}
        </p>
      ) : null}

      <ul className="list-none divide-y divide-ui-border p-0">
        {approval.updates.map((update) => (
          <li className="flex flex-wrap items-center justify-between gap-3 px-4 py-3" key={update.reference}>
            <div className="min-w-0">
              <p className="flex items-center gap-1.5 break-all font-mono text-[11px] text-muted">
                {update.reference.startsWith("refs/tags/")
                  ? <Tag aria-hidden="true" className="size-3.5 shrink-0" />
                  : <GitBranch aria-hidden="true" className="size-3.5 shrink-0" />}
                {referenceKind(update.reference)} {displayReference(update.reference)}
              </p>
              <p className="mt-1 break-all font-mono text-[10px] text-subtle">
                {update.previousObject ?? "new ref"}
                <span aria-hidden="true"> → </span>
                {update.proposedObject}
              </p>
            </div>
            {update.rewrites ? (
              <Badge size="xs" tone="warning">
                <AlertTriangle aria-hidden="true" className="mr-1 size-3" /> Rewrite
              </Badge>
            ) : null}
          </li>
        ))}
      </ul>

      <div className="flex justify-end gap-2 border-t border-ui-border bg-canvas/40 px-4 py-3">
        <Button size="small" variant="danger" disabled={publishing} onClick={onReject}>
          <X aria-hidden="true" className="size-3.5" />
          Reject
        </Button>
        <Button size="small" variant="primary" disabled={publishing} onClick={onApprove}>
          {publishing
            ? <LoaderCircle aria-hidden="true" className="size-3.5 animate-spin" />
            : <Check aria-hidden="true" className="size-3.5" />}
          {publishing ? "Publishing…" : "Approve and publish"}
        </Button>
      </div>
    </li>
  );
}

function RepositoryCard({
  repository,
  disabled,
  refreshing,
  onRefresh,
}: {
  repository: repositories.Repository;
  disabled: boolean;
  refreshing: boolean;
  onRefresh: () => void;
}) {
  const presentation = cachePresentation(repository.cache);
  return (
    <li className="overflow-hidden rounded-xl border border-ui-border bg-surface/60">
      <div className="flex items-start justify-between gap-3 border-b border-ui-border px-4 py-3">
        <div className="min-w-0">
          <h3 className="truncate font-mono text-xs font-semibold text-foreground">
            /workspace/{repository.path}
          </h3>
          <p className="mt-1 break-all font-mono text-[10px] leading-4 text-subtle">
            {repository.source}
          </p>
        </div>
        <div className="flex shrink-0 items-center gap-2">
          <Badge size="xs" tone={presentation.tone}>{presentation.label}</Badge>
          <Button
            aria-label={`Refresh /workspace/${repository.path}`}
            disabled={disabled}
            size="small"
            onClick={onRefresh}
          >
            {refreshing
              ? <LoaderCircle aria-hidden="true" className="size-3.5 animate-spin" />
              : <RefreshCw aria-hidden="true" className="size-3.5" />}
            {refreshing ? "Refreshing…" : "Refresh"}
          </Button>
        </div>
      </div>

      {repository.cache.status === "Ready" ? (
        <dl className="grid grid-cols-2 gap-px bg-ui-border sm:grid-cols-4">
          <CacheMetric
            icon={<GitBranch aria-hidden="true" className="size-3.5" />}
            label="Branches"
            value={formatCount(repository.cache.branches)}
          />
          <CacheMetric
            icon={<Tag aria-hidden="true" className="size-3.5" />}
            label="Tags"
            value={formatCount(repository.cache.tags)}
          />
          <CacheMetric
            icon={<Boxes aria-hidden="true" className="size-3.5" />}
            label="Captured refs"
            value={formatCount(repository.cache.captures)}
          />
          <CacheMetric
            icon={<Database aria-hidden="true" className="size-3.5" />}
            label="Cache size"
            value={formatBytes(repository.cache.sizeBytes)}
          />
          <div className="col-span-2 bg-canvas/50 px-3 py-2.5 text-[10px] text-subtle sm:col-span-4">
            Cache version {formatCount(repository.cache.version)}
            <span aria-hidden="true"> · </span>
            {shortId(repository.cache.cacheId)}
            <span aria-hidden="true"> · </span>
            {repository.cache.versionUpdatedAt
              ? `version updated ${formatTimestamp(repository.cache.versionUpdatedAt)}`
              : "version time unavailable"}
            <br />
            {repository.cache.refreshedAt
              ? `Last checked ${formatTimestamp(repository.cache.refreshedAt)}`
              : "Not checked successfully"}
            <br />
            {formatCount(addCounts(repository.cache.looseObjects, repository.cache.packedObjects))} objects
            <span aria-hidden="true"> · </span>
            {formatCount(repository.cache.packs)} packs
            {Number(repository.cache.garbageBytes) > 0 ? (
              <>
                <span aria-hidden="true"> · </span>
                {formatBytes(repository.cache.garbageBytes)} reclaimable
              </>
            ) : null}
          </div>
          {repository.cache.refreshError ? (
            <p
              className="col-span-2 bg-red-500/5 px-3 py-2.5 text-[10px] text-red-200 sm:col-span-4"
              role="alert"
            >
              Latest refresh failed: {repository.cache.refreshError}
            </p>
          ) : null}
        </dl>
      ) : repository.cache.status === "Failed" ? (
        <p className="px-4 py-3 text-xs leading-5 text-red-200" role="alert">
          {repository.cache.message}
        </p>
      ) : (
        <p className="px-4 py-3 text-xs leading-5 text-subtle">
          The cache will be created when Tascarrel first synchronizes or captures this repository.
        </p>
      )}
    </li>
  );
}

function CacheMetric({
  icon,
  label,
  value,
}: {
  icon: ReactNode;
  label: string;
  value: string;
}) {
  return (
    <div className="bg-canvas/50 px-3 py-3">
      <dt className="flex items-center gap-1.5 text-[10px] text-subtle">{icon}{label}</dt>
      <dd className="mt-1 font-mono text-xs text-muted">{value}</dd>
    </div>
  );
}

function cachePresentation(cache: repositories.RepositoryCacheState): {
  label: string;
  tone: BadgeTone;
} {
  if (cache.status === "Ready") return { label: "Cached", tone: "success" };
  if (cache.status === "Failed") return { label: "Cache error", tone: "danger" };
  return { label: "Not cached", tone: "muted" };
}

function addCounts(left: unknown, right: unknown): number {
  return Number(left) + Number(right);
}

function formatCount(value: unknown): string {
  const count = Number(value);
  return Number.isFinite(count) ? count.toLocaleString() : String(value);
}

function formatBytes(value: unknown): string {
  const bytes = Number(value);
  if (!Number.isFinite(bytes)) return String(value);
  const units = ["B", "KiB", "MiB", "GiB", "TiB"];
  let amount = Math.max(0, bytes);
  let unit = 0;
  while (amount >= 1024 && unit < units.length - 1) {
    amount /= 1024;
    unit += 1;
  }
  return `${amount >= 10 || unit === 0 ? amount.toFixed(0) : amount.toFixed(1)} ${units[unit]}`;
}

function displayReference(reference: string): string {
  return reference.replace(/^refs\/(heads|tags)\//, "");
}

function referenceKind(reference: string): string {
  return reference.startsWith("refs/tags/") ? "Tag" : "Branch";
}

function shortId(id: unknown): string {
  const value = String(id);
  const suffix = value.split("_").at(-1) ?? value;
  return suffix.slice(0, 8);
}

function formatTimestamp(value: unknown): string {
  const timestamp = new Date(String(value));
  return Number.isNaN(timestamp.valueOf()) ? String(value) : timestamp.toLocaleString();
}

function errorMessage(cause: unknown): string {
  return cause instanceof Error ? cause.message : String(cause);
}
