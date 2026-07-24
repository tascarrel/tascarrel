import { Activity, Cpu, HardDrive, MemoryStick, Server } from "lucide-react";

import type { config, workspaces } from "../../api/generated/index.ts";
import { Sparkline, type SparklinePoint } from "../../components/ui/Sparkline.tsx";
import {
  GUEST_METRIC_HISTORY_WINDOW_MS,
  useGuestInformation,
  useGuestMetrics,
} from "../guest/state.ts";
import { useWorkspaceConfig } from "../workspaces/runtimeState.ts";
import { WorkspaceVmLog } from "../workspaces/WorkspaceVmLog.tsx";

export function WorkspaceRuntimeSettings({ workspace }: { workspace: workspaces.Workspace }) {
  return workspace.state.status === "Running"
    ? <RunningWorkspaceRuntimeSettings workspace={workspace} />
    : <OfflineWorkspaceRuntimeSettings workspace={workspace} />;
}

function RunningWorkspaceRuntimeSettings({ workspace }: { workspace: workspaces.Workspace }) {
  const configState = useWorkspaceConfig(workspace.name);
  const guestInstanceId = workspace.state.status === "Running"
    || workspace.state.status === "Starting"
    || workspace.state.status === "Stopping"
    || workspace.state.status === "Failed"
    ? workspace.state.guestInstanceId
    : undefined;
  const informationState = useGuestInformation(workspace.name, guestInstanceId);
  const metricState = useGuestMetrics(workspace.name);
  const information = informationState.value;
  const latestMetric = metricState.value?.at(-1);
  const metricHistory = latestMetric
    ? (metricState.value ?? []).filter(
        (sample) => sample.cursor.guestInstanceId === latestMetric.cursor.guestInstanceId,
      )
    : [];

  return (
    <div className="max-w-5xl space-y-5">
      <div>
        <h2 className="text-sm font-semibold text-foreground">Runtime</h2>
        <p className="mt-1 max-w-2xl text-xs leading-5 text-subtle">
          Current VM resources, workspace configuration, resource metrics, and retained console output.
        </p>
      </div>

      {configState.error || informationState.error || metricState.error ? (
        <p className="rounded-xl border border-red-500/20 bg-red-500/5 px-3 py-2 text-xs text-red-200" role="alert">
          {configState.error?.message ?? informationState.error?.message ?? metricState.error?.message}
        </p>
      ) : null}

      <section aria-labelledby="runtime-resources-title">
        <h3 className="mb-2 text-xs font-medium text-muted" id="runtime-resources-title">VM Resources</h3>
        <div className="grid gap-2 sm:grid-cols-2 xl:grid-cols-4">
          <MetricCard
            icon={Cpu}
            label="CPU"
            value={latestMetric ? `${formatNumber(latestMetric.cpu.usagePercent, 1)}%` : "—"}
            detail={information ? `${information.logicalProcessorCount} logical processors` : "Loading capacity…"}
            history={metricHistory.map((sample) => ({
              timestamp: Date.parse(String(sample.observedAt)),
              value: Number(sample.cpu.usagePercent),
            }))}
          />
          <MetricCard
            icon={MemoryStick}
            label="Memory available"
            value={latestMetric ? formatBytes(latestMetric.memory.availableBytes) : "—"}
            detail={information ? `${formatBytes(information.memoryTotalBytes)} total` : "Loading capacity…"}
            history={metricHistory.map((sample) => ({
              timestamp: Date.parse(String(sample.observedAt)),
              value: Number(sample.memory.availableBytes),
            }))}
          />
          <MetricCard
            icon={HardDrive}
            label="State disk available"
            value={latestMetric ? formatBytes(latestMetric.stateDisk.availableBytes) : "—"}
            detail={information ? `${formatBytes(information.stateDiskTotalBytes)} total` : "Loading capacity…"}
            history={metricHistory.map((sample) => ({
              timestamp: Date.parse(String(sample.observedAt)),
              value: Number(sample.stateDisk.availableBytes),
            }))}
          />
          <MetricCard
            icon={Activity}
            label="Uptime"
            value={latestMetric ? formatDuration(latestMetric.uptimeSeconds) : "—"}
            detail={latestMetric ? `Load ${formatNumber(latestMetric.cpu.loadAverage.oneMinute, 2)}` : "Waiting for metrics…"}
          />
        </div>
        {information ? (
          <dl className="mt-2 grid gap-3 rounded-xl border border-ui-border bg-surface/60 p-3 text-[11px] sm:grid-cols-2 lg:grid-cols-4">
            <div><dt className="text-subtle">Guest instance</dt><dd className="mt-1 break-all font-mono text-muted">{information.guestInstanceId}</dd></div>
            <div><dt className="text-subtle">Latest sample</dt><dd className="mt-1 text-muted">{latestMetric ? formatTimestamp(latestMetric.observedAt) : "Waiting…"}</dd></div>
            <div><dt className="text-subtle">Load averages</dt><dd className="mt-1 text-muted">{latestMetric ? [latestMetric.cpu.loadAverage.oneMinute, latestMetric.cpu.loadAverage.fiveMinutes, latestMetric.cpu.loadAverage.fifteenMinutes].map((value) => formatNumber(value, 2)).join(" · ") : "Waiting…"}</dd></div>
            <div><dt className="text-subtle">Swap free</dt><dd className="mt-1 text-muted">{latestMetric ? `${formatBytes(latestMetric.memory.swapFreeBytes)} of ${formatBytes(latestMetric.memory.swapTotalBytes)}` : "Waiting…"}</dd></div>
          </dl>
        ) : null}
        {information && Object.keys(information.properties).length > 0 ? (
          <details className="mt-2 rounded-xl border border-ui-border bg-surface/40 p-3 text-[11px]">
            <summary className="cursor-pointer text-muted">Guest properties</summary>
            <dl className="mt-3 grid gap-2 sm:grid-cols-2">
              {Object.entries(information.properties).map(([key, value]) => (
                <div key={key}><dt className="font-mono text-subtle">{key}</dt><dd className="mt-1 break-all font-mono text-muted">{formatProperty(value)}</dd></div>
              ))}
            </dl>
          </details>
        ) : null}
      </section>

      <WorkspaceConfigSummary state={configState.value} loading={!configState.ready} />
      {guestInstanceId ? <WorkspaceVmLog guestInstanceId={guestInstanceId} /> : null}
    </div>
  );
}

function OfflineWorkspaceRuntimeSettings({ workspace }: { workspace: workspaces.Workspace }) {
  const configState = useWorkspaceConfig(workspace.name);
  const guestInstanceId = workspace.state.status === "Starting"
    || workspace.state.status === "Stopping"
    || workspace.state.status === "Failed"
    ? workspace.state.guestInstanceId
    : undefined;
  return (
    <div className="max-w-5xl space-y-5">
      <div>
        <h2 className="text-sm font-semibold text-foreground">Runtime</h2>
        <p className="mt-1 text-xs leading-5 text-subtle">
          Workspace state: {workspace.state.status}. Guest information and metrics are available while it is running.
        </p>
      </div>
      {workspace.state.status === "Failed" ? (
        <p className="rounded-xl border border-red-500/20 bg-red-500/5 px-3 py-2 text-xs text-red-200" role="alert">
          {workspace.state.message}
        </p>
      ) : null}
      {configState.error ? <p className="text-xs text-red-200" role="alert">{configState.error.message}</p> : null}
      <WorkspaceConfigSummary state={configState.value} loading={!configState.ready} />
      {guestInstanceId ? <WorkspaceVmLog guestInstanceId={guestInstanceId} /> : null}
    </div>
  );
}

function MetricCard({
  icon: Icon,
  label,
  value,
  detail,
  history,
  historyLabel,
}: {
  icon: typeof Cpu;
  label: string;
  value: string;
  detail: string;
  history?: readonly SparklinePoint[];
  historyLabel?: string;
}) {
  return (
    <div className="rounded-xl border border-ui-border bg-surface/70 p-3">
      <div className="flex items-center gap-2 text-[11px] text-subtle">
        <Icon aria-hidden="true" className="size-3.5" /> {label}
      </div>
      <strong className="mt-2 block text-sm font-semibold text-foreground">{value}</strong>
      <span className="mt-1 block text-[10px] text-subtle">{detail}</span>
      {history ? (
        <Sparkline
          label={historyLabel ?? `${label} over the last five minutes`}
          points={history}
          windowMs={GUEST_METRIC_HISTORY_WINDOW_MS}
          className="mt-2 h-7 w-full overflow-visible text-accent-text"
        />
      ) : null}
    </div>
  );
}

function WorkspaceConfigSummary({
  state,
  loading,
}: {
  state?: config.ConfigChangedEvent;
  loading: boolean;
}) {
  const config = state?.config;
  return (
    <section className="rounded-xl border border-ui-border bg-surface/50 p-4" aria-labelledby="workspace-config-title">
      <div className="flex items-center justify-between gap-3">
        <h3 className="flex items-center gap-2 text-xs font-medium text-muted" id="workspace-config-title">
          <Server aria-hidden="true" className="size-3.5" /> Workspace Configuration
        </h3>
        <span className="text-[10px] text-subtle">{state ? formatTimestamp(state.modifiedAt) : loading ? "Loading…" : "Unavailable"}</span>
      </div>
      {state?.lastConfigError ? (
        <p className="mt-3 text-xs text-red-200" role="alert">{state.lastConfigError.message}</p>
      ) : config ? (
        <dl className="mt-3 grid gap-3 text-[11px] sm:grid-cols-2 lg:grid-cols-3">
          <ConfigValue label="VM" value={formatVmConfig(config.vm)} />
          <ConfigValue label="Repositories" value={String(Object.keys(config.repos ?? {}).length)} />
          <ConfigValue label="Caches" value={String(config.caches?.length ?? 0)} />
          <ConfigValue label="Environment entries" value={String(Object.keys(config.env ?? {}).length)} />
          <ConfigValue label="Setup steps" value={String(config.setup?.steps?.length ?? 0)} />
          <ConfigValue label="Init steps" value={String(config.init?.steps?.length ?? 0)} />
        </dl>
      ) : (
        <p className="mt-3 text-xs text-subtle">{loading ? "Loading workspace configuration…" : "No valid configuration has been observed."}</p>
      )}
      {state?.lastSettingsError ? (
        <p className="mt-3 text-xs text-red-200" role="alert">{state.lastSettingsError.message}</p>
      ) : null}
    </section>
  );
}

function ConfigValue({ label, value }: { label: string; value: string }) {
  return <div><dt className="text-subtle">{label}</dt><dd className="mt-1 text-muted">{value}</dd></div>;
}

function formatVmConfig(vm: config.WorkspaceVmConfig | undefined): string {
  if (!vm) return "Defaults";
  return [vm.cores ? `${vm.cores} cores` : undefined, vm.memory, vm.disk ? `${vm.disk} disk` : undefined]
    .filter(Boolean)
    .join(" · ") || "Defaults";
}

function formatNumber(value: unknown, digits: number): string {
  const number = Number(value);
  return Number.isFinite(number) ? number.toFixed(digits) : String(value);
}

function formatBytes(value: unknown): string {
  const bytes = Number(value);
  if (!Number.isFinite(bytes)) return String(value);
  const units = ["B", "KiB", "MiB", "GiB", "TiB"];
  let amount = bytes;
  let unit = 0;
  while (amount >= 1024 && unit < units.length - 1) {
    amount /= 1024;
    unit += 1;
  }
  return `${amount >= 10 || unit === 0 ? amount.toFixed(0) : amount.toFixed(1)} ${units[unit]}`;
}

function formatDuration(value: unknown): string {
  const seconds = Number(value);
  if (!Number.isFinite(seconds)) return String(value);
  const days = Math.floor(seconds / 86_400);
  const hours = Math.floor((seconds % 86_400) / 3_600);
  const minutes = Math.floor((seconds % 3_600) / 60);
  return days > 0 ? `${days}d ${hours}h` : hours > 0 ? `${hours}h ${minutes}m` : `${minutes}m`;
}

function formatProperty(value: unknown): string {
  if (typeof value === "string") return value;
  try {
    return JSON.stringify(value);
  } catch {
    return String(value);
  }
}

function formatTimestamp(value: string): string {
  const date = new Date(value);
  return Number.isNaN(date.getTime()) ? value : date.toLocaleString();
}
