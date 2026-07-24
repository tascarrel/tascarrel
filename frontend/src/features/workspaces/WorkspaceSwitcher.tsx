import { LoaderCircle, Play, Square } from "lucide-react";
import type { ReactNode } from "react";

import type { workspaces } from "../../api/generated/index.ts";
import { Badge } from "../../components/ui/Badge.tsx";
import { IconButtonGroup } from "../../components/ui/IconButtonGroup.tsx";
import { SelectControl } from "../../components/ui/SelectControl.tsx";
import { WorkspaceUsbPopover } from "./WorkspaceUsbPopover.tsx";

export type WorkspaceLifecycleOperation = "start" | "stop";
export type PendingWorkspaceAction = {
  workspace: workspaces.WorkspaceName;
  operation: WorkspaceLifecycleOperation;
};

export function WorkspaceSwitcher({
  value,
  workspaces,
  pendingAction,
  actionsDisabled,
  usbEnabled,
  onChange,
  onStart,
  onStop,
}: {
  value?: workspaces.WorkspaceName;
  workspaces: readonly workspaces.Workspace[];
  pendingAction?: PendingWorkspaceAction;
  actionsDisabled: boolean;
  usbEnabled: boolean;
  onChange: (value: workspaces.WorkspaceName) => void;
  onStart: (workspace: workspaces.Workspace) => void;
  onStop: (workspace: workspaces.Workspace) => void;
}) {
  const selectedWorkspace = workspaces.find((workspace) => workspace.name === value);
  if (workspaces.length > MAXIMUM_INLINE_WORKSPACES) {
    return (
      <div className="workspace-switcher-stack">
        <SelectControl
          className="w-full"
          label="Workspace"
          value={value ?? ""}
          options={workspaces.map((workspace) => ({
            label: workspace.name,
            value: workspace.name,
            badge: {
              label: workspace.state.status,
              tone: workspace.state.status === "Running" ? "success" : "muted",
            },
          }))}
          hideLabel
          variant="sidebar"
          onChange={(next) => onChange(next as workspaces.WorkspaceName)}
        />
        {selectedWorkspace ? (
          <WorkspaceTab
            workspace={selectedWorkspace}
            selected
            pendingOperation={pendingAction?.workspace === selectedWorkspace.name
              ? pendingAction.operation
              : undefined}
            actionsDisabled={actionsDisabled}
            usbEnabled={usbEnabled}
            onSelect={onChange}
            onStart={onStart}
            onStop={onStop}
          />
        ) : null}
      </div>
    );
  }

  return (
    <div className="workspace-radio-group" role="group" aria-label="Workspace">
      {workspaces.map((workspace) => (
        <WorkspaceTab
          workspace={workspace}
          selected={workspace.name === value}
          pendingOperation={pendingAction?.workspace === workspace.name
            ? pendingAction.operation
            : undefined}
          actionsDisabled={actionsDisabled}
          usbEnabled={usbEnabled}
          key={workspace.name}
          onSelect={onChange}
          onStart={onStart}
          onStop={onStop}
        />
      ))}
      {workspaces.length === 0 ? (
        <span className="px-2 py-3 text-xs text-subtle">Waiting for workspaces…</span>
      ) : null}
    </div>
  );
}

function WorkspaceTab({
  workspace,
  selected,
  pendingOperation,
  actionsDisabled,
  usbEnabled,
  onSelect,
  onStart,
  onStop,
}: {
  workspace: workspaces.Workspace;
  selected: boolean;
  pendingOperation?: WorkspaceLifecycleOperation;
  actionsDisabled: boolean;
  usbEnabled: boolean;
  onSelect: (workspace: workspaces.WorkspaceName) => void;
  onStart: (workspace: workspaces.Workspace) => void;
  onStop: (workspace: workspaces.Workspace) => void;
}) {
  const hasLifecycleActions = workspace.state.status === "Stopped"
    || workspace.state.status === "Running";
  return (
    <div className={`workspace-tab ${selected ? "workspace-tab-selected" : ""} ${hasLifecycleActions ? "workspace-tab-has-actions" : ""} ${selected && usbEnabled ? "workspace-tab-usb-enabled" : ""}`}>
      <button
        className="workspace-radio-option"
        type="button"
        aria-pressed={selected}
        title={`${workspace.name} · ${workspace.state.status}`}
        onClick={() => onSelect(workspace.name)}
      >
        <span className="workspace-radio-label">{workspace.name}</span>
        <WorkspaceStatusBadge status={workspace.state.status} />
      </button>
      <WorkspaceLifecycleControls
        workspace={workspace}
        pendingOperation={pendingOperation}
        disabled={actionsDisabled}
        usbEnabled={selected && usbEnabled}
        onStart={() => onStart(workspace)}
        onStop={() => onStop(workspace)}
      />
    </div>
  );
}

function WorkspaceLifecycleControls({
  workspace,
  pendingOperation,
  disabled,
  usbEnabled,
  onStart,
  onStop,
}: {
  workspace: workspaces.Workspace;
  pendingOperation?: WorkspaceLifecycleOperation;
  disabled: boolean;
  usbEnabled: boolean;
  onStart: () => void;
  onStop: () => void;
}) {
  const canStart = workspace.state.status === "Stopped";
  const canStop = workspace.state.status === "Running";
  if (!canStart && !canStop) return null;

  return (
    <IconButtonGroup
      className="workspace-lifecycle-actions"
      label={`Lifecycle actions for ${workspace.name}`}
    >
      {canStop && usbEnabled ? <WorkspaceUsbPopover workspace={workspace.name} disabled={disabled} /> : null}
      {canStart ? (
        <WorkspaceLifecycleButton
          label={`Start ${workspace.name}`}
          pending={pendingOperation === "start"}
          disabled={disabled}
          onClick={onStart}
        >
          <Play aria-hidden="true" size={12} />
        </WorkspaceLifecycleButton>
      ) : null}
      {canStop ? (
        <WorkspaceLifecycleButton
          label={`Stop ${workspace.name}`}
          pending={pendingOperation === "stop"}
          disabled={disabled}
          onClick={onStop}
        >
          <Square aria-hidden="true" size={11} />
        </WorkspaceLifecycleButton>
      ) : null}
    </IconButtonGroup>
  );
}

function WorkspaceLifecycleButton({
  children,
  label,
  pending,
  disabled,
  onClick,
}: {
  children: ReactNode;
  label: string;
  pending: boolean;
  disabled: boolean;
  onClick: () => void;
}) {
  return (
    <button
      className="workspace-lifecycle-button"
      type="button"
      aria-label={label}
      title={label}
      disabled={disabled}
      onClick={onClick}
    >
      {pending ? <LoaderCircle aria-hidden="true" className="animate-spin" size={12} /> : children}
    </button>
  );
}

function WorkspaceStatusBadge({ status }: { status: workspaces.WorkspaceState["status"] }) {
  return <Badge className="workspace-status-badge" size="xs" tone={status === "Running" ? "success" : "muted"}>{status}</Badge>;
}

const MAXIMUM_INLINE_WORKSPACES = 3;
