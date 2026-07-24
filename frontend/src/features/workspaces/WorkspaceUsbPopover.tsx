import { Cable, LoaderCircle, Unplug } from "lucide-react";
import { useState } from "react";

import { hostApi } from "../../api/client.ts";
import type { workspaces } from "../../api/generated/index.ts";
import { Popover } from "../../components/ui/Popover.tsx";
import { useUsbDevices } from "./runtimeState.ts";

export function WorkspaceUsbPopover({
  workspace,
  disabled,
}: {
  workspace: workspaces.WorkspaceName;
  disabled: boolean;
}) {
  const devicesState = useUsbDevices(workspace);
  const [pendingDevice, setPendingDevice] = useState<workspaces.UsbDeviceId>();
  const [error, setError] = useState<string>();
  const inventory = devicesState.value;

  const updateDevice = async (device: workspaces.UsbDevice) => {
    if (pendingDevice) return;
    setPendingDevice(device.id);
    setError(undefined);
    try {
      if (device.forwardedTo === workspace) {
        await hostApi.execute("workspaces_DetachUsbDevice", { workspace, deviceId: device.id });
      } else {
        await hostApi.execute("workspaces_AttachUsbDevice", { workspace, deviceId: device.id });
      }
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setPendingDevice(undefined);
    }
  };

  return (
    <Popover.Root>
      <Popover.Trigger
        className="workspace-lifecycle-button"
        aria-label={`Manage USB devices for ${workspace}`}
        title="USB devices"
        disabled={disabled}
      >
        <Cable aria-hidden="true" size={13} />
      </Popover.Trigger>
      <Popover.Content
        title="USB Devices"
        description={`Attach a host device to ${workspace}.`}
        side="right"
      >
        <div className="workspace-usb-list">
          {inventory ? (
            inventory.supported ? (
              inventory.devices.length > 0 ? inventory.devices.map((device) => (
                <UsbDeviceRow
                  key={device.id}
                  device={device}
                  workspace={workspace}
                  pending={pendingDevice === device.id}
                  disabled={Boolean(pendingDevice)}
                  onClick={() => void updateDevice(device)}
                />
              )) : <p className="workspace-usb-empty">No USB devices connected.</p>
            ) : <p className="workspace-usb-empty">USB forwarding is unavailable on this host.</p>
          ) : <p className="workspace-usb-empty">Discovering USB devices…</p>}
        </div>
        {error ?? devicesState.error?.message ? (
          <p className="workspace-usb-error" role="alert">{error ?? devicesState.error?.message}</p>
        ) : null}
      </Popover.Content>
    </Popover.Root>
  );
}

function UsbDeviceRow({
  device,
  workspace,
  pending,
  disabled,
  onClick,
}: {
  device: workspaces.UsbDevice;
  workspace: workspaces.WorkspaceName;
  pending: boolean;
  disabled: boolean;
  onClick: () => void;
}) {
  const attached = device.forwardedTo === workspace;
  const unavailable = !device.hasRequiredPermissions || Boolean(device.forwardedTo && !attached);
  const detail = device.forwardedTo && !attached
    ? `In use by ${device.forwardedTo}`
    : !device.hasRequiredPermissions
    ? "Your host user lacks USB device permissions"
    : `${hexId(device.vendorId)}:${hexId(device.productId)}${device.serialNumber ? ` · ${device.serialNumber}` : ""}`;
  return (
    <button
      className="workspace-usb-device"
      type="button"
      disabled={disabled || unavailable}
      onClick={onClick}
    >
      <span className="workspace-usb-device-copy">
        <span className="workspace-usb-device-name">{device.name}</span>
        <span className="workspace-usb-device-detail">{detail}</span>
      </span>
      <span className="workspace-usb-device-action">
        {pending
          ? <LoaderCircle aria-hidden="true" className="animate-spin" size={13} />
          : attached
          ? <Unplug aria-hidden="true" size={13} />
          : <Cable aria-hidden="true" size={13} />}
        {attached ? "Detach" : "Attach"}
      </span>
    </button>
  );
}

function hexId(value: number): string {
  return value.toString(16).padStart(4, "0");
}
