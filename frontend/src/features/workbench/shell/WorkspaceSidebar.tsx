import {
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
import { useState, type FormEvent, type ReactNode } from "react";

import type { pods, workspaces } from "../../../api/generated/index.ts";
import { Badge } from "../../../components/ui/Badge.tsx";
import { ConfirmDialog } from "../../../components/ui/ConfirmDialog.tsx";
import { CountBadge } from "../../../components/ui/CountBadge.tsx";
import { IconButtonGroup } from "../../../components/ui/IconButtonGroup.tsx";
import { TascarrelLogo } from "../../../components/ui/TascarrelLogo.tsx";
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
  onCreateWorkspace: (workspace: workspaces.WorkspaceName) => Promise<void>;
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
  const [workspaceCreationOpen, setWorkspaceCreationOpen] = useState(false);
  const [workspaceName, setWorkspaceName] = useState("");
  const [creatingWorkspace, setCreatingWorkspace] = useState(false);
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

  const createWorkspace = async (event: FormEvent) => {
    event.preventDefault();
    const name = workspaceName.trim();
    if (!name || creatingWorkspace) return;
    setCreatingWorkspace(true);
    setWorkspaceLifecycleError(undefined);
    try {
      await onCreateWorkspace(name as workspaces.WorkspaceName);
      setWorkspaceName("");
      setWorkspaceCreationOpen(false);
    } catch (cause) {
      setWorkspaceLifecycleError(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setCreatingWorkspace(false);
    }
  };

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
                aria-label={workspaceCreationOpen ? "Cancel workspace creation" : "Create workspace"}
                aria-pressed={workspaceCreationOpen}
                title={workspaceCreationOpen ? "Cancel workspace creation" : "Create workspace"}
                onClick={() => setWorkspaceCreationOpen((current) => !current)}
              >
                {workspaceCreationOpen ? <X aria-hidden="true" size={13} /> : <Plus aria-hidden="true" size={13} />}
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
          {workspaceCreationOpen ? (
            <form className="sidebar-create-form" onSubmit={(event) => void createWorkspace(event)}>
              <label className="sr-only" htmlFor="workspace-name">Workspace name</label>
              <input
                id="workspace-name"
                autoFocus
                value={workspaceName}
                placeholder="Workspace name"
                pattern="[A-Za-z0-9_\-]{1,64}"
                disabled={creatingWorkspace}
                onChange={(event) => setWorkspaceName(event.target.value)}
              />
              <button type="submit" disabled={creatingWorkspace || !workspaceName.trim()}>
                {creatingWorkspace ? <LoaderCircle aria-hidden="true" className="animate-spin" size={12} /> : "Create"}
              </button>
            </form>
          ) : null}
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
            const pending = pendingPodId === pod.id;
            const attention = attentionPodIds.has(pod.id);
            const running = pod.status.status === "Running";
            const starting = pod.status.status === "Creating"
              || pod.status.status === "Building"
              || pod.status.status === "Starting"
              || pod.status.status === "Initializing";
            const statusLabel = pod.status.status === "Failed"
              ? `Failed: ${pod.status.message}`
              : pod.status.status;
            const label = `${pod.title || "Untitled pod"} · ${statusLabel}${attention ? " · Needs attention" : ""}`;
            return (
              <div
                className={`pod-tab-row ${selected ? "pod-tab-row-active" : ""} ${attention ? "pod-tab-row-attention" : ""} ${selected && agentNeedsInput ? "pod-tab-row-needs-input" : ""} ${pending ? "pod-tab-row-pending" : ""} ${running ? "" : "pod-tab-row-subdued"} ${starting ? "pod-tab-row-starting" : ""}`}
                key={pod.id}
              >
                <button
                  className="pod-tab"
                  type="button"
                  aria-current={selected ? "true" : undefined}
                  aria-label={label}
                  title={pod.title || "Untitled pod"}
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
                  {selected && agentNeedsInput ? (
                    <span className="pod-attention">
                      <Bell aria-hidden="true" size={11} /> Needs input
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
        description={`Destroy ${destroyTarget?.title || "this pod"} and all of its persistent resources? This cannot be undone.`}
        destructive
        open={Boolean(destroyTarget)}
        pending={destroyTarget ? pendingPodId === destroyTarget.id : false}
        title="Destroy Pod?"
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
