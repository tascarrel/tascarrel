import { CircleAlert, CircleStop, LoaderCircle, Power, Trash2 } from "lucide-react";
import { useState } from "react";

import type { guest, workspaces } from "../../api/generated/index.ts";
import type { WorkspaceScreenName } from "../../app/router.tsx";
import { Button } from "../../components/ui/Button.tsx";
import { LifecycleScreenFrame } from "../../components/ui/LifecycleScreenFrame.tsx";
import { WorkspaceVmLog } from "./WorkspaceVmLog.tsx";

export function workspaceScreenForState(
  state: workspaces.WorkspaceState,
): WorkspaceScreenName {
  if (state.status === "Stopped") return "stopped";
  if (state.status === "Starting") return "starting";
  if (state.status === "Stopping") return "stopping";
  if (state.status === "Failed") return "failed";
  if (state.status === "Destroying") return "destroying";
  return "starting";
}

export function WorkspaceLifecycleScreen({
  screen,
  workspace,
  onStart,
}: {
  screen: WorkspaceScreenName;
  workspace: workspaces.Workspace;
  onStart: () => Promise<void>;
}) {
  if (screen === "stopped") {
    return <WorkspaceStoppedScreen workspace={workspace} onStart={onStart} />;
  }
  if (screen === "starting") return <WorkspaceStartingScreen workspace={workspace} />;
  if (screen === "stopping") return <WorkspaceStoppingScreen workspace={workspace} />;
  if (screen === "failed") {
    return <WorkspaceFailedScreen workspace={workspace} onStart={onStart} />;
  }
  return <WorkspaceDestroyingScreen />;
}

export function WorkspaceStoppedScreen({
  workspace,
  onStart,
}: {
  workspace: workspaces.Workspace;
  onStart: () => Promise<void>;
}) {
  return (
    <LifecycleScreenFrame
      icon={<Power aria-hidden="true" className="size-4" />}
      title="Workspace Stopped"
    >
      <WorkspaceStartButton
        enabled={workspace.state.status === "Stopped"}
        label="Start workspace"
        pendingLabel="Starting…"
        onStart={onStart}
      />
    </LifecycleScreenFrame>
  );
}

export function WorkspaceStartingScreen({ workspace }: { workspace: workspaces.Workspace }) {
  return (
    <LifecycleScreenFrame
      icon={<LoaderCircle aria-hidden="true" className="size-4 animate-spin" />}
      title="Starting Workspace"
      log={<WorkspaceVmLog guestInstanceId={guestInstanceId(workspace)} presentation="screen" />}
    />
  );
}

export function WorkspaceStoppingScreen({ workspace }: { workspace: workspaces.Workspace }) {
  return (
    <LifecycleScreenFrame
      icon={<CircleStop aria-hidden="true" className="size-4" />}
      title="Stopping Workspace"
      log={<WorkspaceVmLog guestInstanceId={guestInstanceId(workspace)} presentation="screen" />}
    />
  );
}

export function WorkspaceFailedScreen({
  workspace,
  onStart,
}: {
  workspace: workspaces.Workspace;
  onStart: () => Promise<void>;
}) {
  const failure = workspace.state.status === "Failed"
    ? workspace.state.message
    : "No failure is currently recorded for this workspace.";
  return (
    <LifecycleScreenFrame
      danger
      icon={<CircleAlert aria-hidden="true" className="size-4" />}
      title="Workspace Failed"
      log={<WorkspaceVmLog guestInstanceId={guestInstanceId(workspace)} presentation="screen" />}
    >
      <p className="mx-auto max-w-xl text-xs leading-5 text-red-200" role="alert">
        {failure}
      </p>
      <div className="mt-4">
        <WorkspaceStartButton
          enabled={workspace.state.status === "Failed"}
          label="Restart workspace"
          pendingLabel="Restarting…"
          onStart={onStart}
        />
      </div>
    </LifecycleScreenFrame>
  );
}

export function WorkspaceDestroyingScreen() {
  return (
    <LifecycleScreenFrame
      danger
      icon={<Trash2 aria-hidden="true" className="size-4" />}
      title="Destroying Workspace"
    />
  );
}

function WorkspaceStartButton({
  enabled,
  label,
  pendingLabel,
  onStart,
}: {
  enabled: boolean;
  label: string;
  pendingLabel: string;
  onStart: () => Promise<void>;
}) {
  const [pending, setPending] = useState(false);
  const [error, setError] = useState<string>();
  const canStart = enabled && !pending;
  const start = async () => {
    if (!canStart) return;
    setPending(true);
    setError(undefined);
    try {
      await onStart();
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
      setPending(false);
    }
  };

  return (
    <>
      <Button variant="primary" disabled={!canStart} onClick={() => void start()}>
        {pending ? <LoaderCircle aria-hidden="true" className="size-3.5 animate-spin" /> : null}
        {pending ? pendingLabel : label}
      </Button>
      {error ? <p className="mt-3 text-xs text-red-200" role="alert">{error}</p> : null}
    </>
  );
}

function guestInstanceId(workspace: workspaces.Workspace): guest.GuestInstanceId | undefined {
  if (
    workspace.state.status === "Starting"
    || workspace.state.status === "Running"
    || workspace.state.status === "Stopping"
    || workspace.state.status === "Failed"
  ) return workspace.state.guestInstanceId;
  return undefined;
}
