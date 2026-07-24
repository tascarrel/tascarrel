import { hostApi } from "../../api/client.ts";
import type { config, guest, workspaces } from "../../api/generated/index.ts";
import type { BackendStateDefinition } from "../../shared/state/BackendStateCache.ts";
import { useBackendState } from "../../shared/state/StateCacheProvider.tsx";

export function useWorkspaceConfig(workspace: workspaces.WorkspaceName) {
  return useBackendState(workspaceConfigDefinition(workspace));
}

export function useWorkspaceVmLog(guestInstanceId: guest.GuestInstanceId) {
  return useBackendState(workspaceVmLogDefinition(guestInstanceId));
}

export function useUsbDevices(workspace: workspaces.WorkspaceName) {
  return useBackendState(usbDevicesDefinition(workspace));
}

function workspaceConfigDefinition(
  workspace: workspaces.WorkspaceName,
): BackendStateDefinition<config.ConfigChangedEvent, config.ConfigChangedEvent, never> {
  return {
    key: `host/config/${workspace}`,
    connect: (_cursor, handlers) => hostApi.subscribe(
      "config_Changed",
      { workspaceName: workspace },
      {
        onEvent: handlers.onEvent,
        onState: handlers.onConnection,
        onError: handlers.onError,
      },
    ),
    applyEvent: (_current, event) => ({ value: event }),
  };
}

function workspaceVmLogDefinition(
  guestInstanceId: guest.GuestInstanceId,
): BackendStateDefinition<readonly workspaces.WorkspaceVmLogLine[], workspaces.WorkspaceVmLogEvent, workspaces.WorkspaceVmLogLine["line"]> {
  return {
    key: `host/workspace-vm-log/${guestInstanceId}`,
    retention: "lru",
    connect: (cursor, handlers) => hostApi.subscribe(
      "workspaces_VmLog",
      () => {
        const lastLine = cursor();
        return { guestInstanceId, ...(lastLine === undefined ? {} : { lastLine }) };
      },
      {
        onEvent: handlers.onEvent,
        onState: handlers.onConnection,
        onError: handlers.onError,
      },
    ),
    applyEvent: (current, event) => ({
      value: mergeVmLog(current ?? [], event.lines),
      cursor: event.lines.at(-1)?.line ?? current?.at(-1)?.line,
    }),
  };
}

function usbDevicesDefinition(
  workspace: workspaces.WorkspaceName,
): BackendStateDefinition<workspaces.UsbDevicesChangedEvent, workspaces.UsbDevicesChangedEvent, never> {
  return {
    key: `host/usb-devices/${workspace}`,
    connect: (_cursor, handlers) => hostApi.subscribe(
      "workspaces_UsbDevicesChanged",
      { workspace },
      {
        onEvent: handlers.onEvent,
        onState: handlers.onConnection,
        onError: handlers.onError,
      },
    ),
    applyEvent: (_current, event) => ({ value: event }),
  };
}

function mergeVmLog(
  current: readonly workspaces.WorkspaceVmLogLine[],
  incoming: readonly workspaces.WorkspaceVmLogLine[],
): readonly workspaces.WorkspaceVmLogLine[] {
  const lines = new Map(current.map((line) => [String(line.line), line]));
  for (const line of incoming) lines.set(String(line.line), line);
  return [...lines.values()].sort((left, right) =>
    String(left.line).localeCompare(String(right.line), undefined, { numeric: true })
  );
}
