import {
  AlertTriangle,
  Bell,
  FolderGit2,
  Layers3,
  LoaderCircle,
  Network,
  Play,
  Plus,
  Settings,
  Square,
  Trash2,
  X,
} from "lucide-react";
import { useState, type ReactNode } from "react";

import type { pods, workspaces } from "../../../api/generated/index.ts";
import { Badge } from "../../../components/ui/Badge.tsx";
import { ConfirmDialog } from "../../../components/ui/ConfirmDialog.tsx";
import { CountBadge } from "../../../components/ui/CountBadge.tsx";
import { IconButtonGroup } from "../../../components/ui/IconButtonGroup.tsx";
import { TascarrelLogo } from "../../../components/ui/TascarrelLogo.tsx";
import type { PodChangeSummary } from "../../changes/podChangeSummary.ts";
import {
  WorkspaceSwitcher,
  type PendingWorkspaceAction,
  type WorkspaceLifecycleOperation,
} from "../../workspaces/WorkspaceSwitcher.tsx";
import { SidebarSectionOverline } from "./SidebarSectionOverline.tsx";
import { ShellPanelToggle } from "./ShellPanelToggle.tsx";
import { WorkspaceResourceAlert } from "./WorkspaceResourceAlert.tsx";

export type WorkspaceControlView = "images" | "network" | "repositories" | "settings";

export function WorkspaceSidebar({
  workspaces,
  selectedWorkspace,
  usbEnabled,
  pods,
  podChangeSummaries,
  podChangeSummariesVerified,
  podListEmptyMessage,
  showPodCount,
  selectedPodId,
  busyPodIds,
  attentionPodIds,
  agentNeedsInput,
  repositoryApprovalCount,
  activeWorkspaceView,
  shortcut,
  onSelectWorkspace,
  onCreateWorkspace,
  onStartWorkspace,
  onStopWorkspace,
  onSelectPod,
  canCreatePod,
  podCreationActive,
  onCreatePod,
  onSelectWorkspaceView,
  onStartPod,
  onStopPod,
  onDestroyPod,
  onCollapse,
}: {
  workspaces: readonly workspaces.Workspace[];
  selectedWorkspace?: workspaces.WorkspaceName;
  usbEnabled: boolean;
  pods: readonly pods.Pod[];
  podChangeSummaries: ReadonlyMap<pods.PodId, PodChangeSummary>;
  podChangeSummariesVerified: boolean;
  podListEmptyMessage?: string;
  showPodCount: boolean;
  selectedPodId?: pods.PodId;
  busyPodIds: ReadonlySet<pods.PodId>;
  attentionPodIds: ReadonlySet<pods.PodId>;
  agentNeedsInput: boolean;
  repositoryApprovalCount?: number;
  activeWorkspaceView?: WorkspaceControlView;
  shortcut: ReadonlyArray<string>;
  onSelectWorkspace: (workspace: workspaces.WorkspaceName) => void;
  onCreateWorkspace: () => void;
  onStartWorkspace: (workspace: workspaces.WorkspaceName) => Promise<void>;
  onStopWorkspace: (workspace: workspaces.WorkspaceName) => Promise<void>;
  onSelectPod: (podId: pods.PodId) => void;
  canCreatePod: boolean;
  podCreationActive: boolean;
  onCreatePod: () => void;
  onSelectWorkspaceView: (view: WorkspaceControlView) => void;
  onStartPod: (podId: pods.PodId) => Promise<void>;
  onStopPod: (podId: pods.PodId) => Promise<void>;
  onDestroyPod: (podId: pods.PodId) => Promise<void>;
  onCollapse: () => void;
}) {
  const [pendingPodId, setPendingPodId] = useState<pods.PodId>();
  const [destroyTarget, setDestroyTarget] = useState<pods.Pod>();
  const [lifecycleError, setLifecycleError] = useState<string>();
  const [pendingWorkspaceAction, setPendingWorkspaceAction] = useState<PendingWorkspaceAction>();
  const [workspaceLifecycleError, setWorkspaceLifecycleError] = useState<string>();
  const selectedWorkspaceState = workspaces.find((workspace) => workspace.name === selectedWorkspace);
  const workspaceControlsAvailable = workspaces.some(
    (workspace) => workspace.name === selectedWorkspace && workspace.state.status === "Running",
  );
  const workspaceControlsTitle = workspaceControlsAvailable
    ? undefined
    : "Start the workspace to use workspace tools";
  const sidebarPods = pods.toSorted((left, right) =>
    String(right.createdAt).localeCompare(String(left.createdAt)),
  );

  const runWorkspaceLifecycle = async (
    workspace: workspaces.Workspace,
    operation: WorkspaceLifecycleOperation,
  ) => {
    if (pendingWorkspaceAction) return;
    setWorkspaceLifecycleError(undefined);
    setPendingWorkspaceAction({ workspace: workspace.name, operation });
    try {
      if (operation === "start") await onStartWorkspace(workspace.name);
      else await onStopWorkspace(workspace.name);
    } catch (cause) {
      setWorkspaceLifecycleError(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setPendingWorkspaceAction(undefined);
    }
  };

  const runLifecycle = async (
    pod: pods.Pod,
    operation: (podId: pods.PodId) => Promise<void>,
  ) => {
    if (pendingPodId) return;
    setLifecycleError(undefined);
    setPendingPodId(pod.id);
    try {
      await operation(pod.id);
      setDestroyTarget(undefined);
    } catch (cause) {
      setLifecycleError(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setPendingPodId(undefined);
    }
  };

  return (
    <>
      <aside className="workspace-sidebar" id="workspace-sidebar" aria-label="Workspace navigation">
        <div className="workspace-sidebar-header">
          <div className="workspace-brand-row">
            <div className="workspace-brand">
              <TascarrelLogo className="workspace-brand-logo" />
              <span className="workspace-brand-name">
                Tascarrel
                <Badge className="workspace-brand-badge" size="xs">Alpha</Badge>
              </span>
            </div>
            <div className="workspace-brand-actions">
              <button
                className="sidebar-create-button"
                type="button"
                aria-label="Create workspace"
                title="Create workspace"
                onClick={onCreateWorkspace}
              >
                <Plus aria-hidden="true" size={13} />
              </button>
              <ShellPanelToggle
                side="left"
                expanded
                label="workspace sidebar"
                shortcut={shortcut}
                shortcutKeys="Control+B Meta+B"
                aria-controls="workspace-sidebar"
                onClick={onCollapse}
              />
            </div>
          </div>
          <WorkspaceSwitcher
            value={selectedWorkspace}
            workspaces={workspaces}
            pendingAction={pendingWorkspaceAction}
            actionsDisabled={Boolean(pendingWorkspaceAction)}
            usbEnabled={usbEnabled}
            onChange={onSelectWorkspace}
            onStart={(workspace) => void runWorkspaceLifecycle(workspace, "start")}
            onStop={(workspace) => void runWorkspaceLifecycle(workspace, "stop")}
          />
          <WorkspaceResourceAlert workspace={selectedWorkspaceState} />
          {workspaceLifecycleError ? (
            <p className="workspace-switcher-error" role="alert">{workspaceLifecycleError}</p>
          ) : null}
        </div>
        <div className="pod-list">
          <SidebarSectionOverline
            actions={(
              <>
                {showPodCount ? <span className="pod-count">{pods.length}</span> : null}
                <button
                  className="sidebar-create-button"
                  type="button"
                  aria-label={podCreationActive ? "Cancel pod creation" : "Create pod"}
                  aria-pressed={podCreationActive}
                  title={canCreatePod ? (podCreationActive ? "Cancel pod creation" : "Create pod") : "Start the workspace to create a pod"}
                  disabled={!canCreatePod}
                  onClick={onCreatePod}
                >
                  {podCreationActive ? <X aria-hidden="true" size={12} /> : <Plus aria-hidden="true" size={12} />}
                </button>
              </>
            )}
          >
            Pods
          </SidebarSectionOverline>
          {sidebarPods.map((pod) => {
            const selected = pod.id === selectedPodId;
            const changeSummary = podChangeSummaries.get(pod.id);
            const changesLabel = changeSummary
              ? podChangesLabel(changeSummary)
              : undefined;
            const pending = pendingPodId === pod.id;
            const attention = attentionPodIds.has(pod.id);
            const attentionLabel = attention
              ? selected && agentNeedsInput ? "Needs input" : "Needs attention"
              : undefined;
            const running = pod.status.status === "Running";
            const starting = pod.status.status === "Creating"
              || pod.status.status === "Building"
              || pod.status.status === "Starting"
              || pod.status.status === "Initializing";
            const statusLabel = pod.status.status === "Failed"
              ? `Failed: ${pod.status.message}`
              : pod.status.status;
            const label = `${pod.title || "Untitled pod"} · ${statusLabel}${attentionLabel ? ` · ${attentionLabel}` : ""}${changesLabel ? ` · ${changesLabel}` : ""}`;
            return (
              <div
                className={`pod-tab-row ${selected ? "pod-tab-row-active" : ""} ${attention ? "pod-tab-row-attention" : ""} ${selected && agentNeedsInput ? "pod-tab-row-needs-input" : ""} ${pending ? "pod-tab-row-pending" : ""} ${running ? "" : "pod-tab-row-subdued"} ${starting ? "pod-tab-row-starting" : ""}`}
                data-actions={pod.status.status !== "Destroying" ? true : undefined}
                key={pod.id}
              >
                <button
                  className="pod-tab"
                  type="button"
                  aria-current={selected ? "true" : undefined}
                  aria-label={label}
                  title={label}
                  onClick={() => onSelectPod(pod.id)}
                >
                  <span className="pod-tab-copy">
                    <span className="pod-tab-title">
                      {busyPodIds.has(pod.id) ? (
                        <LoaderCircle
                          className="animate-spin pod-busy-indicator"
                          size={11}
                          aria-label="Agent working"
                        />
                      ) : null}
                      <span>{pod.title || "Untitled pod"}</span>
                    </span>
                  </span>
                  {changeSummary && changeSummary.changedFileCount > 0 && !attention ? (
                    <CountBadge
                      aria-hidden="true"
                      className="pod-change-count"
                      count={changeSummary.changedFileCount}
                      size="xs"
                      tone={changeSummary.conflictCount > 0 ? "danger" : "muted"}
                      title={changesLabel}
                    />
                  ) : null}
                  {attentionLabel ? (
                    <span className="pod-attention" aria-hidden="true" title={attentionLabel}>
                      <Bell aria-hidden="true" size={11} />
                    </span>
                  ) : null}
                </button>
                <PodLifecycleControls
                  pod={pod}
                  pending={pending}
                  disabled={Boolean(pendingPodId)}
                  onStart={() => void runLifecycle(pod, onStartPod)}
                  onStop={() => void runLifecycle(pod, onStopPod)}
                  onDestroy={() => setDestroyTarget(pod)}
                />
              </div>
            );
          })}
          {pods.length === 0 ? (
            <p className="px-3 py-4 text-xs text-subtle">
              {podListEmptyMessage ?? "No pods in this workspace."}
            </p>
          ) : null}
        </div>
        <nav className="workspace-controls" aria-label="Workspace controls">
          <SidebarSectionOverline>Workspace</SidebarSectionOverline>
          <button
            className="workspace-control-button"
            type="button"
            data-active={activeWorkspaceView === "repositories" || undefined}
            aria-current={activeWorkspaceView === "repositories" ? "page" : undefined}
            disabled={!workspaceControlsAvailable}
            title={workspaceControlsTitle}
            onClick={() => onSelectWorkspaceView("repositories")}
          >
            <FolderGit2 aria-hidden="true" size={14} />
            <span>Repositories</span>
            {repositoryApprovalCount ? (
              <CountBadge
                className="ml-auto"
                count={repositoryApprovalCount}
                aria-label={`${repositoryApprovalCount} unresolved approval ${repositoryApprovalCount === 1 ? "request" : "requests"}`}
              />
            ) : null}
          </button>
          <button
            className="workspace-control-button"
            type="button"
            data-active={activeWorkspaceView === "images" || undefined}
            aria-current={activeWorkspaceView === "images" ? "page" : undefined}
            disabled={!workspaceControlsAvailable}
            title={workspaceControlsTitle}
            onClick={() => onSelectWorkspaceView("images")}
          >
            <Layers3 aria-hidden="true" size={14} />
            <span>Images</span>
          </button>
          <button
            className="workspace-control-button"
            type="button"
            data-active={activeWorkspaceView === "network" || undefined}
            aria-current={activeWorkspaceView === "network" ? "page" : undefined}
            disabled={!workspaceControlsAvailable}
            title={workspaceControlsTitle}
            onClick={() => onSelectWorkspaceView("network")}
          >
            <Network aria-hidden="true" size={14} />
            <span>Network</span>
          </button>
          <button
            className="workspace-control-button"
            type="button"
            data-active={activeWorkspaceView === "settings" || undefined}
            aria-current={activeWorkspaceView === "settings" ? "page" : undefined}
            disabled={!workspaceControlsAvailable}
            title={workspaceControlsTitle}
            onClick={() => onSelectWorkspaceView("settings")}
          >
            <Settings aria-hidden="true" size={14} />
            <span>Settings</span>
          </button>
          {lifecycleError ? (
            <p className="workspace-control-error" role="alert">{lifecycleError}</p>
          ) : null}
        </nav>
      </aside>
      <ConfirmDialog
        confirmLabel="Destroy pod"
        description={destroyTarget ? (
          <DestroyPodWarning
            pod={destroyTarget}
            summary={podChangeSummaries.get(destroyTarget.id)}
            verified={podChangeSummariesVerified}
          />
        ) : null}
        destructive
        open={Boolean(destroyTarget)}
        pending={destroyTarget ? pendingPodId === destroyTarget.id : false}
        title={destroyTarget ? <DestroyPodTitle pod={destroyTarget} /> : "Destroy pod"}
        onOpenChange={(open) => {
          if (!open) setDestroyTarget(undefined);
        }}
        onConfirm={() => {
          if (destroyTarget) void runLifecycle(destroyTarget, onDestroyPod);
        }}
      />
    </>
  );
}

function podChangesLabel(summary: PodChangeSummary): string {
  const details: string[] = [];
  if (summary.changedFileCount > 0) {
    const files = `${summary.changedFileCount} changed ${summary.changedFileCount === 1 ? "file" : "files"}`;
    const repositories = `${summary.dirtyRepositoryCount} ${summary.dirtyRepositoryCount === 1 ? "repository" : "repositories"}`;
    const conflicts = summary.conflictCount > 0
      ? `, including ${summary.conflictCount} ${summary.conflictCount === 1 ? "conflict" : "conflicts"}`
      : "";
    details.push(`${files} in ${repositories}${conflicts}`);
  }
  if (summary.unpushedCommitCount > 0) {
    details.push(
      `${summary.unpushedCommitCount} unpushed ${summary.unpushedCommitCount === 1 ? "commit" : "commits"}`,
    );
  }
  return details.join(" · ");
}

function DestroyPodTitle({ pod }: { pod: pods.Pod }) {
  return (
    <span className="flex min-w-0 items-center gap-3">
      <span className="grid size-9 shrink-0 place-items-center rounded-full bg-red-500/10 text-red-300">
        <Trash2 aria-hidden="true" className="size-4" />
      </span>
      <span className="min-w-0">
        <span className="block text-xs font-medium text-red-300">Destroy pod</span>
        <span className="mt-0.5 block truncate text-lg font-semibold text-foreground">
          {pod.title || "Untitled pod"}
        </span>
      </span>
    </span>
  );
}

function DestroyPodWarning({
  pod,
  summary,
  verified,
}: {
  pod: pods.Pod;
  summary?: PodChangeSummary;
  verified: boolean;
}) {
  const hasLocalWork = Boolean(
    summary?.changedFileCount || summary?.unpushedCommitCount,
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

function PodLifecycleControls({
  pod,
  pending,
  disabled,
  onStart,
  onStop,
  onDestroy,
}: {
  pod: pods.Pod;
  pending: boolean;
  disabled: boolean;
  onStart: () => void;
  onStop: () => void;
  onDestroy: () => void;
}) {
  const canStart = pod.status.status === "Stopped";
  const canStop = pod.status.status === "Running";
  const canDestroy = pod.status.status !== "Destroying";
  const hasAction = canStart || canStop || canDestroy;
  if (!hasAction) return null;

  return (
    <IconButtonGroup
      className="pod-lifecycle-actions"
      data-pending={pending || undefined}
      label={`Lifecycle actions for ${pod.title || "untitled pod"}`}
    >
      {pending ? (
        <span className="pod-lifecycle-button" aria-label={`Updating ${pod.title || "untitled pod"}`}>
          <LoaderCircle aria-hidden="true" className="animate-spin" size={13} />
        </span>
      ) : (
        <>
          {canStart ? (
            <PodLifecycleButton label={`Start ${pod.title || "untitled pod"}`} disabled={disabled} onClick={onStart}>
              <Play aria-hidden="true" size={12} />
            </PodLifecycleButton>
          ) : null}
          {canStop ? (
            <PodLifecycleButton label={`Stop ${pod.title || "untitled pod"}`} disabled={disabled} onClick={onStop}>
              <Square aria-hidden="true" size={11} />
            </PodLifecycleButton>
          ) : null}
          {canDestroy ? (
            <PodLifecycleButton danger label={`Destroy ${pod.title || "untitled pod"}`} disabled={disabled} onClick={onDestroy}>
              <Trash2 aria-hidden="true" size={12} />
            </PodLifecycleButton>
          ) : null}
        </>
      )}
    </IconButtonGroup>
  );
}

function PodLifecycleButton({
  children,
  danger = false,
  disabled,
  label,
  onClick,
}: {
  children: ReactNode;
  danger?: boolean;
  disabled: boolean;
  label: string;
  onClick: () => void;
}) {
  return (
    <button
      className={`pod-lifecycle-button ${danger ? "pod-lifecycle-button-danger" : ""}`}
      type="button"
      aria-label={label}
      title={label}
      disabled={disabled}
      onClick={onClick}
    >
      {children}
    </button>
  );
}
