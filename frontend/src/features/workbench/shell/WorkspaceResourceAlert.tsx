import { TriangleAlert } from "lucide-react";

import type { workspaces } from "../../../api/generated/index.ts";
import { useGuestInformation, useGuestMetrics } from "../../guest/state.ts";

const CRITICAL_UTILIZATION_PERCENT = 95;

export function WorkspaceResourceAlert({ workspace }: { workspace?: workspaces.Workspace }) {
  if (workspace?.state.status !== "Running") return null;
  return <RunningWorkspaceResourceAlert workspace={workspace} />;
}

function RunningWorkspaceResourceAlert({ workspace }: { workspace: workspaces.Workspace }) {
  const guestInstanceId = workspace.state.status === "Running"
    ? workspace.state.guestInstanceId
    : undefined;
  const metricState = useGuestMetrics(workspace.name);
  const informationState = useGuestInformation(workspace.name, guestInstanceId);
  const latest = (metricState.value ?? [])
    .filter((sample) => sample.cursor.guestInstanceId === guestInstanceId)
    .at(-1);
  const memoryTotalBytes = Number(informationState.value?.memoryTotalBytes);
  const stateDiskTotalBytes = Number(informationState.value?.stateDiskTotalBytes);
  const memoryUtilization = latest && memoryTotalBytes > 0
    ? utilization(memoryTotalBytes - Number(latest.memory.availableBytes), memoryTotalBytes)
    : undefined;
  const diskUtilization = latest && stateDiskTotalBytes > 0
    ? utilization(stateDiskTotalBytes - Number(latest.stateDisk.availableBytes), stateDiskTotalBytes)
    : undefined;
  const criticalResources = [
    criticalResource("Memory", memoryUtilization),
    criticalResource("Disk", diskUtilization),
  ].filter((resource) => resource !== undefined);

  if (criticalResources.length === 0) return null;

  return (
    <div className="workspace-resource-alert" role="alert" aria-label="Critical workspace resource usage">
      <TriangleAlert className="workspace-resource-alert-icon" aria-hidden="true" size={15} />
      <div className="workspace-resource-alert-copy">
        <strong className="workspace-resource-alert-title">
          {criticalResources.length === 1
            ? `${criticalResources[0]?.label} usage critical`
            : "Resource usage critical"}
        </strong>
        <span className="workspace-resource-alert-detail">
          {criticalResources.map((resource) => `${resource.label} ${formatUtilization(resource.utilization)}`).join(" · ")}
        </span>
      </div>
    </div>
  );
}

function criticalResource(label: string, utilizationPercent: number | undefined) {
  return utilizationPercent !== undefined && utilizationPercent > CRITICAL_UTILIZATION_PERCENT
    ? { label, utilization: utilizationPercent }
    : undefined;
}

function formatUtilization(utilizationPercent: number): string {
  return `${utilizationPercent.toFixed(1)}% used`;
}

function utilization(used: number, total: number): number {
  if (!Number.isFinite(used) || !Number.isFinite(total) || total <= 0) return 0;
  return Math.min(100, Math.max(0, used / total * 100));
}
