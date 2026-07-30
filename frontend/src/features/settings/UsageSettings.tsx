import {
  Archive,
  Pencil,
  Plus,
  RotateCcw,
  Save,
  X,
} from "lucide-react";
import { useEffect, useMemo, useState, type FormEvent } from "react";

import { hostApi } from "../../api/client.ts";
import type {
  chats,
  common,
  config,
  workspaces,
} from "../../api/generated/index.ts";
import { Button } from "../../components/ui/Button.tsx";
import { SelectControl } from "../../components/ui/SelectControl.tsx";
import { TextInput } from "../../components/ui/TextInput.tsx";
import { useWorkspaceConfig } from "../workspaces/runtimeState.ts";
import { sameWorkspaceSettings } from "./settingsComparison.ts";
import { UsageReport } from "./UsageReport.tsx";

const UNASSIGNED = ":unassigned";
const MAX_COST_CENTER_ID_LENGTH = 64;

type CostCenterEntry = {
  id: string;
  costCenter: config.WorkspaceCostCenter;
};

type CostCenterDraft = {
  id: string;
  name: string;
  existing: boolean;
};

export function UsageSettings({
  workspace,
  running,
}: {
  workspace: workspaces.WorkspaceName;
  running: boolean;
}) {
  const configState = useWorkspaceConfig(workspace);
  const [actionError, setActionError] = useState<string>();
  const [pendingSettings, setPendingSettings] = useState<config.WorkspaceSettings>();
  const [savingSettings, setSavingSettings] = useState(false);
  const [draft, setDraft] = useState<CostCenterDraft>();
  const [month, setMonth] = useState(currentLocalMonth);

  const observedSettings = configState.value?.settings;
  const configInstanceId = configState.value?.configInstanceId;
  const settings = pendingSettings ?? observedSettings ?? {};
  const usage = settings.usage ?? {};
  const entries = costCenterEntries(usage.costCenters);
  const activeEntries = entries.filter(({ costCenter }) => costCenter.archived !== true);
  const mutationDisabled = savingSettings
    || pendingSettings !== undefined
    || !configInstanceId
    || Boolean(configState.value?.lastSettingsError);
  const interval = useMemo(() => localMonthInterval(month), [month]);

  useEffect(() => {
    if (
      pendingSettings !== undefined
      && sameWorkspaceSettings(configState.value?.settings, pendingSettings)
    ) setPendingSettings(undefined);
  }, [configState.value?.settings, pendingSettings]);

  useEffect(() => {
    setDraft(undefined);
    setActionError(undefined);
    setMonth(currentLocalMonth());
  }, [workspace]);

  const persistUsage = async (nextUsage: config.WorkspaceUsageSettings) => {
    if (configState.value?.lastSettingsError) {
      throw new Error("Fix settings.json before changing usage settings in the UI.");
    }
    if (!configInstanceId) {
      throw new Error("Workspace configuration is not ready yet.");
    }
    const nextSettings: config.WorkspaceSettings = {
      ...settings,
      usage: nextUsage,
    };
    setActionError(undefined);
    setPendingSettings(nextSettings);
    setSavingSettings(true);
    try {
      await hostApi.execute("config_UpdateSettings", {
        workspaceName: workspace,
        configInstanceId,
        settings: nextSettings,
      });
    } catch (cause) {
      setPendingSettings(undefined);
      throw cause;
    } finally {
      setSavingSettings(false);
    }
  };

  const saveCostCenter = async (event: FormEvent) => {
    event.preventDefault();
    if (!draft || mutationDisabled) return;
    const id = draft.id.trim();
    const name = draft.name.trim();
    const validationError = validateCostCenter(id, name, draft.existing, usage.costCenters);
    if (validationError) {
      setActionError(validationError);
      return;
    }
    const previous = usage.costCenters?.[id];
    try {
      await persistUsage({
        ...usage,
        costCenters: {
          ...(usage.costCenters ?? {}),
          [id]: {
            name,
            ...(previous?.archived === true ? { archived: true } : {}),
          },
        },
      });
      setDraft(undefined);
    } catch (cause) {
      setActionError(errorMessage(cause));
    }
  };

  const setArchived = async (id: string, archived: boolean) => {
    const costCenter = usage.costCenters?.[id];
    if (!costCenter || mutationDisabled) return;
    const nextUsage = {
      ...usage,
      costCenters: {
        ...(usage.costCenters ?? {}),
        [id]: {
          ...costCenter,
          ...(archived ? { archived: true } : { archived: undefined }),
        },
      },
    };
    if (archived && usage.defaultCostCenter === id) {
      delete nextUsage.defaultCostCenter;
    }
    try {
      await persistUsage(nextUsage);
    } catch (cause) {
      setActionError(errorMessage(cause));
    }
  };

  const setDefault = async (value: string) => {
    const nextUsage = { ...usage };
    if (value === UNASSIGNED) {
      delete nextUsage.defaultCostCenter;
    } else {
      nextUsage.defaultCostCenter = value as chats.ChatCostCenterId;
    }
    try {
      await persistUsage(nextUsage);
    } catch (cause) {
      setActionError(errorMessage(cause));
    }
  };

  const error = actionError
    ?? configState.error?.message
    ?? configState.value?.lastSettingsError?.message;

  return (
    <div className="max-w-5xl">
      <div>
        <h2 className="text-sm font-semibold text-foreground">Usage</h2>
        <p className="mt-1 max-w-2xl text-xs leading-5 text-subtle">
          Attribute chat token consumption and locally calculated cost to workspace cost centers.
          Assignments apply to the whole history of a chat.
        </p>
      </div>

      {error ? (
        <p
          className="mt-4 rounded-xl border border-red-500/20 bg-red-500/5 px-3 py-2 text-xs text-red-200"
          role="alert"
        >
          {error}
        </p>
      ) : null}

      <section className="mt-6" aria-labelledby="usage-cost-centers">
        <div className="flex flex-wrap items-end justify-between gap-3">
          <div>
            <h3 className="text-xs font-semibold text-foreground" id="usage-cost-centers">
              Cost centers
            </h3>
            <p className="mt-1 text-[11px] leading-5 text-subtle">
              IDs are stable attribution keys. Names can be changed later.
            </p>
          </div>
          <Button
            size="small"
            disabled={mutationDisabled || Boolean(draft)}
            onClick={() => setDraft({ id: "", name: "", existing: false })}
          >
            <Plus aria-hidden="true" className="size-3.5" />
            Add cost center
          </Button>
        </div>

        {draft ? (
          <form
            className="mt-4 grid gap-3 rounded-xl border border-ui-border bg-surface/40 p-4 sm:grid-cols-[minmax(0,0.8fr)_minmax(0,1.2fr)_auto]"
            onSubmit={(event) => void saveCostCenter(event)}
          >
            <label className="flex min-w-0 flex-col gap-1 text-[10px] text-subtle">
              Stable ID
              <TextInput
                autoFocus={!draft.existing}
                disabled={draft.existing || mutationDisabled}
                maxLength={MAX_COST_CENTER_ID_LENGTH}
                placeholder="client_alpha"
                value={draft.id}
                onChange={(event) => setDraft({ ...draft, id: event.target.value })}
              />
            </label>
            <label className="flex min-w-0 flex-col gap-1 text-[10px] text-subtle">
              Display name
              <TextInput
                autoFocus={draft.existing}
                disabled={mutationDisabled}
                placeholder="Client Alpha"
                value={draft.name}
                onChange={(event) => setDraft({ ...draft, name: event.target.value })}
              />
            </label>
            <div className="flex items-end gap-2">
              <Button size="small" variant="primary" disabled={mutationDisabled} type="submit">
                <Save aria-hidden="true" className="size-3.5" />
                Save
              </Button>
              <Button
                aria-label="Cancel editing cost center"
                size="icon"
                disabled={mutationDisabled}
                onClick={() => setDraft(undefined)}
              >
                <X aria-hidden="true" className="size-3.5" />
              </Button>
            </div>
          </form>
        ) : null}

        <div className="mt-4 overflow-hidden rounded-xl border border-ui-border">
          {entries.length ? (
            <ul className="divide-y divide-ui-border">
              {entries.map(({ id, costCenter }) => (
                <li
                  className="flex flex-wrap items-center justify-between gap-3 bg-surface/30 px-3 py-2.5"
                  key={id}
                >
                  <div className="min-w-0">
                    <p className="truncate text-xs font-medium text-foreground">
                      {costCenter.name}
                      {costCenter.archived === true ? (
                        <span className="ml-2 text-[10px] font-normal text-subtle">Archived</span>
                      ) : null}
                    </p>
                    <p className="mt-0.5 truncate font-mono text-[10px] text-subtle">{id}</p>
                  </div>
                  <div className="flex gap-2">
                    <Button
                      aria-label={`Edit ${costCenter.name}`}
                      size="icon"
                      disabled={mutationDisabled || Boolean(draft)}
                      title="Edit name"
                      onClick={() => setDraft({
                        id,
                        name: costCenter.name,
                        existing: true,
                      })}
                    >
                      <Pencil aria-hidden="true" className="size-3.5" />
                    </Button>
                    <Button
                      aria-label={`${costCenter.archived === true ? "Restore" : "Archive"} ${costCenter.name}`}
                      size="icon"
                      disabled={mutationDisabled}
                      title={costCenter.archived === true ? "Restore cost center" : "Archive cost center"}
                      onClick={() => void setArchived(id, costCenter.archived !== true)}
                    >
                      {costCenter.archived === true
                        ? <RotateCcw aria-hidden="true" className="size-3.5" />
                        : <Archive aria-hidden="true" className="size-3.5" />}
                    </Button>
                  </div>
                </li>
              ))}
            </ul>
          ) : (
            <p className="bg-surface/30 px-4 py-5 text-xs text-subtle">
              No cost centers configured. Usage remains available under Unassigned.
            </p>
          )}
        </div>

        <div className="mt-4 max-w-sm">
          <SelectControl
            label="Default for new chats"
            value={usage.defaultCostCenter ?? UNASSIGNED}
            options={[
              { value: UNASSIGNED, label: "Unassigned" },
              ...activeEntries.map(({ id, costCenter }) => ({
                value: id,
                label: costCenter.name,
              })),
            ]}
            disabled={mutationDisabled}
            onChange={(value) => void setDefault(value)}
          />
        </div>
      </section>

      <section className="mt-8 border-t border-ui-border pt-6" aria-labelledby="usage-report">
        <div className="flex flex-wrap items-end justify-between gap-3">
          <div>
            <h3 className="text-xs font-semibold text-foreground" id="usage-report">
              Monthly usage
            </h3>
            <p className="mt-1 text-[11px] leading-5 text-subtle">
              Includes active and archived chats, grouped by their current assignment.
            </p>
          </div>
          <label className="flex flex-col gap-1 text-[10px] text-subtle">
            Month
            <TextInput
              className="w-40"
              type="month"
              value={month}
              onChange={(event) => setMonth(event.target.value)}
            />
          </label>
        </div>

        {!running ? (
          <p className="mt-4 rounded-xl border border-ui-border bg-surface/30 px-4 py-5 text-xs text-subtle">
            Start the workspace to load its usage report. Cost-center settings can still be edited.
          </p>
        ) : interval ? (
          <UsageReport
            workspace={workspace}
            from={interval.from}
            until={interval.until}
            usageSettings={usage}
          />
        ) : (
          <p className="mt-4 text-xs text-red-200">Choose a valid month.</p>
        )}
      </section>
    </div>
  );
}

function costCenterEntries(
  costCenters: config.WorkspaceUsageSettings["costCenters"],
): CostCenterEntry[] {
  return Object.entries(costCenters ?? {})
    .filter((entry): entry is [string, config.WorkspaceCostCenter] => entry[1] !== undefined)
    .map(([id, costCenter]) => ({ id, costCenter }))
    .sort((left, right) =>
      Number(left.costCenter.archived === true) - Number(right.costCenter.archived === true)
      || left.costCenter.name.localeCompare(right.costCenter.name)
      || left.id.localeCompare(right.id)
    );
}

function validateCostCenter(
  id: string,
  name: string,
  existing: boolean,
  costCenters: config.WorkspaceUsageSettings["costCenters"],
): string | undefined {
  if (
    !id
    || id.length > MAX_COST_CENTER_ID_LENGTH
    || !/^[A-Za-z0-9_-]+$/.test(id)
  ) {
    return "Use a 1–64 character ID containing only letters, numbers, hyphens, or underscores.";
  }
  if (!name) return "Enter a display name.";
  if (!existing && costCenters?.[id]) return `Cost center “${id}” already exists.`;
  return undefined;
}

function localMonthInterval(
  value: string,
): { from: common.Timestamp; until: common.Timestamp } | undefined {
  const match = /^(\d{4})-(\d{2})$/.exec(value);
  if (!match) return undefined;
  const year = Number(match[1]);
  const month = Number(match[2]);
  if (month < 1 || month > 12) return undefined;
  const from = new Date(year, month - 1, 1);
  const until = new Date(year, month, 1);
  if (Number.isNaN(from.getTime()) || Number.isNaN(until.getTime())) return undefined;
  return {
    from: from.toISOString() as common.Timestamp,
    until: until.toISOString() as common.Timestamp,
  };
}

function currentLocalMonth(): string {
  const now = new Date();
  return `${now.getFullYear()}-${String(now.getMonth() + 1).padStart(2, "0")}`;
}

function errorMessage(cause: unknown): string {
  return cause instanceof Error ? cause.message : String(cause);
}
