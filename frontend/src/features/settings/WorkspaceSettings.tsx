import {
  ArrowDown,
  ArrowUp,
  Eye,
  EyeOff,
  LogOut,
  RefreshCw,
  Star,
  Trash2,
} from "lucide-react";
import { useEffect, useState } from "react";

import { guestApi, hostApi } from "../../api/client.ts";
import type { chats, config, workspaces } from "../../api/generated/index.ts";
import { Button } from "../../components/ui/Button.tsx";
import { ConfirmDialog } from "../../components/ui/ConfirmDialog.tsx";
import { SelectControl } from "../../components/ui/SelectControl.tsx";
import { SidebarTabs, SidebarTabsPanel } from "../../components/ui/SidebarTabs.tsx";
import { HarnessConnectionCard } from "../chat/components/HarnessAuthPanel.tsx";
import { ModelControls } from "../chat/components/ModelControls.tsx";
import { harnessLabel } from "../chat/model/format.ts";
import {
  chatModelPreferences,
  preferredDefaultModelSelection,
  visibleChatModels,
  withChatModelPreferences,
} from "../chat/model/modelPreferences.ts";
import { useChatHarnesses } from "../chat/state.ts";
import { useWorkspaceConfig } from "../workspaces/runtimeState.ts";
import { SecretsSettings } from "../secrets/SecretsSettings.tsx";
import { McpSettings } from "./McpSettings.tsx";
import { sameWorkspaceSettings } from "./settingsComparison.ts";
import { TasciSettings } from "./TasciSettings.tsx";
import { UsageSettings } from "./UsageSettings.tsx";
import { WorkspaceRuntimeSettings } from "./WorkspaceRuntimeSettings.tsx";
import { RemoteAccessSettings } from "./RemoteAccessSettings.tsx";

export function WorkspaceSettings({ workspace }: { workspace: workspaces.Workspace }) {
  const running = workspace.state.status === "Running";

  return (
    <div className="flex h-full min-h-0 flex-col overflow-hidden bg-canvas text-foreground">
      <SidebarTabs
        ariaLabel="Workspace setting sections"
        defaultValue="runtime"
        items={[
          {
            value: "runtime",
            label: "Runtime",
          },
          ...(running
            ? [{
                value: "harnesses",
                label: "Harnesses",
              }]
            : []),
          {
            value: "mcp",
            label: "MCP",
          },
          {
            value: "tasci",
            label: "Tasci",
          },
          {
            value: "usage",
            label: "Usage",
          },
          {
            value: "secrets",
            label: "Secrets",
          },
          {
            value: "remote-access",
            label: "Remote Access",
          },
          {
            value: "danger",
            label: "Danger Zone",
            tone: "danger",
          },
        ]}
      >
        <SidebarTabsPanel value="runtime">
          <WorkspaceRuntimeSettings workspace={workspace} />
        </SidebarTabsPanel>

        {running ? (
          <SidebarTabsPanel value="harnesses">
            <HarnessSettings workspace={workspace.name} />
          </SidebarTabsPanel>
        ) : null}

        <SidebarTabsPanel value="mcp">
          <McpSettings workspace={workspace.name} />
        </SidebarTabsPanel>

        <SidebarTabsPanel value="tasci">
          <TasciSettings workspace={workspace.name} />
        </SidebarTabsPanel>

        <SidebarTabsPanel value="usage">
          <UsageSettings workspace={workspace.name} running={running} />
        </SidebarTabsPanel>

        <SidebarTabsPanel value="secrets">
          <SecretsSettings workspace={workspace.name} />
        </SidebarTabsPanel>

        <SidebarTabsPanel value="remote-access">
          <RemoteAccessSettings />
        </SidebarTabsPanel>

        <SidebarTabsPanel value="danger">
          <WorkspaceDangerSettings workspace={workspace.name} />
        </SidebarTabsPanel>
      </SidebarTabs>
    </div>
  );
}

const AUTOMATIC_DEFAULT_HARNESS = "__automatic_default_harness__";

function WorkspaceDangerSettings({ workspace }: { workspace: workspaces.WorkspaceName }) {
  const [confirmationOpen, setConfirmationOpen] = useState(false);
  const [destroying, setDestroying] = useState(false);
  const [error, setError] = useState<string>();

  const destroy = async () => {
    if (destroying) return;
    setDestroying(true);
    setError(undefined);
    try {
      await hostApi.execute("workspaces_Destroy", { workspace });
      setConfirmationOpen(false);
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
      setConfirmationOpen(false);
    } finally {
      setDestroying(false);
    }
  };

  return (
    <>
      <div className="max-w-4xl">
        <h2 className="text-sm font-semibold text-foreground">Danger Zone</h2>
        <p className="mt-1 max-w-2xl text-xs leading-5 text-subtle">
          Destructive workspace operations cannot be undone.
        </p>
        {error ? (
          <p className="mt-4 rounded-xl border border-red-500/20 bg-red-500/5 px-3 py-2 text-xs text-red-200" role="alert">
            {error}
          </p>
        ) : null}
        <section className="mt-5 flex flex-col gap-4 rounded-xl border border-red-500/20 bg-red-500/[0.03] p-4 sm:flex-row sm:items-center sm:justify-between" aria-labelledby="destroy-workspace-title">
          <div>
            <h3 className="text-xs font-semibold text-foreground" id="destroy-workspace-title">
              Destroy Workspace
            </h3>
            <p className="mt-1 max-w-xl text-[11px] leading-5 text-subtle">
              Permanently remove {workspace}, its configuration, and its VM state partition.
            </p>
          </div>
          <Button className="shrink-0" variant="danger" disabled={destroying} onClick={() => setConfirmationOpen(true)}>
            <Trash2 aria-hidden="true" className="size-3.5" />
            Destroy workspace
          </Button>
        </section>
      </div>
      <ConfirmDialog
        confirmLabel="Destroy workspace"
        description={`Destroy ${workspace}, its configuration, and its VM state partition? This cannot be undone.`}
        destructive
        open={confirmationOpen}
        pending={destroying}
        title="Destroy Workspace?"
        onOpenChange={setConfirmationOpen}
        onConfirm={() => void destroy()}
      />
    </>
  );
}

function HarnessSettings({ workspace }: { workspace: workspaces.WorkspaceName }) {
  const harnessState = useChatHarnesses(workspace);
  const configState = useWorkspaceConfig(workspace);
  const [actionError, setActionError] = useState<string>();
  const [pendingSettings, setPendingSettings] = useState<config.WorkspaceSettings>();
  const [savingSettings, setSavingSettings] = useState(false);
  const api = guestApi(workspace);
  const reportError = (cause: unknown) => {
    setActionError(cause instanceof Error ? cause.message : String(cause));
  };
  const observedSettings = configState.value?.settings;
  const configInstanceId = configState.value?.configInstanceId;
  const settings = pendingSettings ?? observedSettings ?? {};
  const settingsPending = savingSettings
    || pendingSettings !== undefined
    || !configInstanceId
    || Boolean(configState.value?.lastSettingsError);

  useEffect(() => {
    if (
      pendingSettings !== undefined
      && sameWorkspaceSettings(configState.value?.settings, pendingSettings)
    ) setPendingSettings(undefined);
  }, [configState.value?.settings, pendingSettings]);

  const persistSettings = async (nextSettings: config.WorkspaceSettings) => {
    if (configState.value?.lastSettingsError) {
      setActionError("Fix settings.json before changing settings in the UI.");
      return;
    }
    if (!configInstanceId) {
      setActionError("Workspace configuration is not ready yet.");
      return;
    }
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
      reportError(cause);
    } finally {
      setSavingSettings(false);
    }
  };
  const updateDefaultHarness = async (value: string) => {
    const chat = { ...(settings.chat ?? {}) };
    if (value === AUTOMATIC_DEFAULT_HARNESS) {
      delete chat.defaultHarness;
    } else {
      chat.defaultHarness = value as chats.ChatHarnessKind;
    }
    await persistSettings({ ...settings, chat });
  };
  const updatePreferences = (
    harness: chats.ChatHarnessKind,
    preferences: config.WorkspaceChatModelPreferences,
  ) => persistSettings(withChatModelPreferences(settings, harness, preferences));

  const accountActions = {
    onInstall: async (harness: chats.ChatHarnessKind) => {
      await api.execute("chats_InstallHarness", { harness });
    },
    onValidate: async (harness: chats.ChatHarnessKind) => {
      await api.execute("chats_ValidateHarnessCredentials", { harness });
    },
    onStart: async (request: chats.ChatHarnessAuthRequest) => {
      await api.execute("chats_StartHarnessAuth", { request });
    },
    onCancel: async (harness: chats.ChatHarnessKind) => {
      await api.execute("chats_CancelHarnessAuth", { harness });
    },
    onLogout: async (harness: chats.ChatHarnessKind) => {
      await api.execute("chats_LogoutHarness", { harness });
    },
  };

  return (
    <div className="max-w-4xl">
      {actionError || harnessState.error || configState.error || configState.value?.lastSettingsError ? (
        <p className="mb-4 rounded-xl border border-red-500/20 bg-red-500/5 px-3 py-2 text-xs text-red-200" role="alert">
          {actionError
            ?? harnessState.error?.message
            ?? configState.error?.message
            ?? configState.value?.lastSettingsError?.message}
        </p>
      ) : null}
      <div className="mb-5">
        <div>
          <h2 className="text-sm font-semibold text-foreground">Harnesses</h2>
          <p className="mt-1 max-w-2xl text-xs leading-5 text-subtle">
            Connect coding harnesses and configure their workspace model preferences. Changes are written to <code>settings.json</code>.
          </p>
        </div>
      </div>

      <section
        aria-labelledby="default-harness-title"
        className="mb-4 rounded-2xl border border-ui-border bg-surface/60 p-4"
      >
        <div className="mb-3">
          <h3 className="text-xs font-medium text-foreground" id="default-harness-title">
            Default Harness
          </h3>
          <p className="mt-1 max-w-2xl text-[11px] leading-5 text-subtle">
            Used when starting a chat if connected; otherwise Tascarrel uses the first connected harness. You can choose another harness before submitting.
          </p>
        </div>
        <SelectControl
          className="w-full sm:max-w-xs"
          disabled={settingsPending || !harnessState.ready}
          label="Default harness"
          options={[
            { label: "First connected harness", value: AUTOMATIC_DEFAULT_HARNESS },
            ...(harnessState.value ?? []).map((harness) => ({
              label: harness.displayName,
              value: harness.kind,
            })),
            ...(settings.chat?.defaultHarness
              && !(harnessState.value ?? []).some(
                (harness) => harness.kind === settings.chat?.defaultHarness,
              )
              ? [{
                  label: harnessLabel(settings.chat.defaultHarness),
                  value: settings.chat.defaultHarness,
                }]
              : []),
          ]}
          value={settings.chat?.defaultHarness ?? AUTOMATIC_DEFAULT_HARNESS}
          onChange={(value) => void updateDefaultHarness(value)}
        />
      </section>

      <div className="grid gap-4">
        {(harnessState.value ?? []).map((harness) =>
          harness.credentials.state === "Valid" ? (
            <ConnectedHarnessSettings
              disabled={settingsPending}
              harness={harness}
              key={harness.kind}
              preferences={chatModelPreferences(settings, harness.kind)}
              onLogout={accountActions.onLogout}
              onValidate={accountActions.onValidate}
              onUpdate={(preferences) => updatePreferences(harness.kind, preferences)}
              onError={reportError}
            />
          ) : (
            <HarnessConnectionCard
              harness={harness}
              key={harness.kind}
              {...accountActions}
              onError={reportError}
            />
          ),
        )}
      </div>
      {!harnessState.ready && !harnessState.value ? (
        <p className="text-xs text-subtle">Loading harness settings…</p>
      ) : null}
    </div>
  );
}

function ConnectedHarnessSettings({
  harness,
  preferences: configuredPreferences,
  disabled,
  onValidate,
  onLogout,
  onUpdate,
  onError,
}: {
  harness: chats.ChatHarness;
  preferences?: config.WorkspaceChatModelPreferences;
  disabled: boolean;
  onValidate: (harness: chats.ChatHarnessKind) => Promise<void>;
  onLogout: (harness: chats.ChatHarnessKind) => Promise<void>;
  onUpdate: (preferences: config.WorkspaceChatModelPreferences) => Promise<void>;
  onError: (cause: unknown) => void;
}) {
  const [accountBusy, setAccountBusy] = useState(false);
  const preferences = configuredPreferences ?? {};
  const defaultSelection = preferredDefaultModelSelection(harness, preferences);
  const orderedModels = visibleChatModels(
    harness,
    { ...preferences, favoriteModels: [], hiddenModels: [] },
  );
  const hidden = new Set(preferences.hiddenModels ?? []);
  const favorites = new Set(preferences.favoriteModels ?? []);
  const account = harness.credentials.state === "Valid"
    ? harness.credentials.email ?? harness.credentials.plan ?? harness.credentials.method
    : "Connected";
  const runAccountAction = async (operation: () => Promise<void>) => {
    setAccountBusy(true);
    try {
      await operation();
    } catch (cause) {
      onError(cause);
    } finally {
      setAccountBusy(false);
    }
  };
  const updateList = (
    field: "favoriteModels" | "hiddenModels",
    model: string,
    included: boolean,
  ) => {
    const values = preferences[field] ?? [];
    const next = included
      ? [...new Set([...values, model])]
      : values.filter((candidate) => candidate !== model);
    void onUpdate({ ...preferences, [field]: next });
  };
  const moveModel = (model: string, direction: -1 | 1) => {
    const order = orderedModels.map((candidate) => candidate.id);
    const index = order.indexOf(model);
    const target = index + direction;
    if (index < 0 || target < 0 || target >= order.length) return;
    [order[index], order[target]] = [order[target], order[index]];
    void onUpdate({ ...preferences, modelOrder: order });
  };

  return (
    <section className="rounded-2xl border border-ui-border bg-surface/60">
      <header className="flex flex-wrap items-start justify-between gap-3 border-b border-ui-border px-4 py-3">
        <div>
          <div className="flex items-center gap-2">
            <span className="size-2 rounded-full bg-emerald-400" aria-label="Connected" />
            <h3 className="text-sm font-semibold text-foreground">{harness.displayName}</h3>
          </div>
          <p className="mt-1 text-[11px] text-subtle">Connected as {account}</p>
        </div>
        <div className="flex gap-2">
          <Button
            size="small"
            disabled={accountBusy || harness.validatingCredentials}
            onClick={() => void runAccountAction(() => onValidate(harness.kind))}
          >
            <RefreshCw className={`size-3 ${harness.validatingCredentials ? "animate-spin" : ""}`} />
            Validate
          </Button>
          <Button
            size="small"
            disabled={accountBusy}
            onClick={() => void runAccountAction(() => onLogout(harness.kind))}
          >
            <LogOut className="size-3" /> Sign out
          </Button>
        </div>
      </header>

      <div className="grid gap-5 p-4">
        <section>
          <div className="mb-3 flex flex-wrap items-end justify-between gap-3">
            <div>
              <h4 className="text-xs font-medium text-foreground">Default Model</h4>
              <p className="mt-1 text-[11px] text-subtle">Used for new chats created with this harness.</p>
            </div>
            {preferences.defaultModel ? (
              <Button
                className="h-auto border-0 bg-transparent px-1 py-1 text-[11px]"
                size="small"
                disabled={disabled}
                onClick={() => {
                  const next = { ...preferences };
                  delete next.defaultModel;
                  void onUpdate(next);
                }}
              >
                Use harness default
              </Button>
            ) : null}
          </div>
          <ModelControls
            harness={harness}
            preferences={preferences}
            selection={defaultSelection}
            disabled={disabled}
            onChange={(selection) => {
              if (selection) void onUpdate({ ...preferences, defaultModel: selection });
            }}
          />
        </section>

        <section>
          <div className="mb-3">
            <h4 className="text-xs font-medium text-foreground">Available Models</h4>
            <p className="mt-1 text-[11px] text-subtle">
              Favourites appear first in model pickers. Reorder models within those groups or hide models you do not use.
            </p>
          </div>
          <div className="overflow-hidden rounded-xl border border-ui-border">
            {orderedModels.map((model, index) => {
              const favorite = favorites.has(model.id);
              const visible = !hidden.has(model.id);
              return (
                <div
                  className="grid grid-cols-[minmax(0,1fr)_auto] items-center gap-3 border-b border-ui-border px-3 py-2.5 last:border-b-0"
                  key={model.id}
                >
                  <div className="min-w-0">
                    <p className="truncate text-xs text-foreground">{model.displayName}</p>
                    <p className="truncate font-mono text-[10px] text-subtle">{model.id}</p>
                  </div>
                  <div className="flex items-center gap-1">
                    <Button
                      aria-label={`Move ${model.displayName} up`}
                      className="size-7 border-0 bg-transparent p-0"
                      size="icon"
                      disabled={disabled || index === 0}
                      title="Move up"
                      onClick={() => moveModel(model.id, -1)}
                    >
                      <ArrowUp className="size-3.5" />
                    </Button>
                    <Button
                      aria-label={`Move ${model.displayName} down`}
                      className="size-7 border-0 bg-transparent p-0"
                      size="icon"
                      disabled={disabled || index === orderedModels.length - 1}
                      title="Move down"
                      onClick={() => moveModel(model.id, 1)}
                    >
                      <ArrowDown className="size-3.5" />
                    </Button>
                    <Button
                      aria-label={`${favorite ? "Remove" : "Add"} ${model.displayName} ${favorite ? "from" : "to"} favourites`}
                      className={`size-7 border-0 bg-transparent p-0 ${favorite ? "text-amber-300" : ""}`}
                      size="icon"
                      disabled={disabled}
                      title={favorite ? "Remove favourite" : "Add favourite"}
                      onClick={() => updateList("favoriteModels", model.id, !favorite)}
                    >
                      <Star className="size-3.5" fill={favorite ? "currentColor" : "none"} />
                    </Button>
                    <Button
                      aria-label={`${visible ? "Hide" : "Show"} ${model.displayName}`}
                      className="size-7 border-0 bg-transparent p-0"
                      size="icon"
                      disabled={disabled}
                      title={visible ? "Hide model" : "Show model"}
                      onClick={() => updateList("hiddenModels", model.id, visible)}
                    >
                      {visible ? <Eye className="size-3.5" /> : <EyeOff className="size-3.5" />}
                    </Button>
                  </div>
                </div>
              );
            })}
            {!orderedModels.length ? (
              <p className="px-3 py-4 text-xs text-subtle">Models will appear after discovery completes.</p>
            ) : null}
          </div>
        </section>
      </div>
    </section>
  );
}
