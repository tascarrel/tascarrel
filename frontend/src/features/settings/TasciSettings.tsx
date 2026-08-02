import { Pencil, Plus, Save, Trash2, X } from "lucide-react";
import { useEffect, useState, type FormEvent } from "react";

import { hostApi } from "../../api/client.ts";
import type { config, workspaces } from "../../api/generated/index.ts";
import { Button } from "../../components/ui/Button.tsx";
import { ConfirmDialog } from "../../components/ui/ConfirmDialog.tsx";
import { SelectControl } from "../../components/ui/SelectControl.tsx";
import { TextInput } from "../../components/ui/TextInput.tsx";
import { useWorkspaceConfig } from "../workspaces/runtimeState.ts";
import { sameWorkspaceSettings } from "./settingsComparison.ts";
import { SettingsField } from "./SettingsField.tsx";
import { TasciPricingEditor } from "./TasciPricingEditor.tsx";
import {
  tasciPricing,
  tasciPricingDraft,
  type TasciPricingDraft,
  validateTasciPricingDraft,
} from "./tasciPricing.ts";

const NO_DEFAULT_MODEL = "__tasci_no_default_model__";

type EndpointEntry = {
  alias: string;
  endpoint: config.WorkspaceTasciEndpoint;
};

type ModelEntry = {
  alias: string;
  model: config.WorkspaceTasciModel;
};

type EndpointDraft = {
  originalAlias?: string;
  alias: string;
  displayName: string;
  baseUrl: string;
  authenticated: boolean;
  header: string;
  value: string;
};

type ModelDraft = {
  originalAlias?: string;
  alias: string;
  displayName: string;
  endpoint: string;
  model: string;
  contextWindow: string;
  maxOutputTokens: string;
  toolCalls: boolean;
  parallelToolCalls: boolean;
  pricing: TasciPricingDraft;
};

type DeleteTarget =
  | { kind: "endpoint"; alias: string }
  | { kind: "model"; alias: string };

export function TasciSettings({ workspace }: { workspace: workspaces.WorkspaceName }) {
  const configState = useWorkspaceConfig(workspace);
  const [actionError, setActionError] = useState<string>();
  const [pendingSettings, setPendingSettings] = useState<config.WorkspaceSettings>();
  const [savingSettings, setSavingSettings] = useState(false);
  const [endpointDraft, setEndpointDraft] = useState<EndpointDraft>();
  const [modelDraft, setModelDraft] = useState<ModelDraft>();
  const [deleteTarget, setDeleteTarget] = useState<DeleteTarget>();
  const [deleting, setDeleting] = useState(false);

  const observedSettings = configState.value?.settings;
  const configInstanceId = configState.value?.configInstanceId;
  const settings = pendingSettings ?? observedSettings ?? {};
  const tasci = settings.chat?.tasci ?? {};
  const endpointEntries = sortedEndpointEntries(tasci.endpoints);
  const modelEntries = sortedModelEntries(tasci.models);
  const mutationDisabled = savingSettings
    || pendingSettings !== undefined
    || !configInstanceId
    || Boolean(configState.value?.lastSettingsError);

  useEffect(() => {
    if (
      pendingSettings !== undefined
      && sameWorkspaceSettings(configState.value?.settings, pendingSettings)
    ) setPendingSettings(undefined);
  }, [configState.value?.settings, pendingSettings]);

  useEffect(() => {
    setEndpointDraft(undefined);
    setModelDraft(undefined);
    setDeleteTarget(undefined);
    setActionError(undefined);
  }, [workspace]);

  const persistTasci = async (nextTasci: config.WorkspaceTasciSettings) => {
    if (configState.value?.lastSettingsError) {
      throw new Error("Fix settings.json before changing Tasci settings in the UI.");
    }
    if (!configInstanceId) {
      throw new Error("Workspace configuration is not ready yet.");
    }
    const nextSettings: config.WorkspaceSettings = {
      ...settings,
      chat: {
        ...(settings.chat ?? {}),
        tasci: nextTasci,
      },
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

  const saveEndpoint = async (event: FormEvent) => {
    event.preventDefault();
    if (!endpointDraft || mutationDisabled) return;
    const validationError = validateEndpointDraft(
      endpointDraft,
      tasci.endpoints,
    );
    if (validationError) {
      setActionError(validationError);
      return;
    }
    const alias = endpointDraft.alias.trim();
    const endpoints = { ...(tasci.endpoints ?? {}) };
    if (endpointDraft.originalAlias && endpointDraft.originalAlias !== alias) {
      delete endpoints[endpointDraft.originalAlias];
    }
    endpoints[alias] = {
      displayName: optionalTrimmed(endpointDraft.displayName),
      protocol: "OpenAiChatCompletions",
      baseUrl: endpointDraft.baseUrl.trim(),
      ...(endpointDraft.authenticated
        ? {
            authorization: {
              header: endpointDraft.header.trim(),
              value: endpointDraft.value,
            },
          }
        : {}),
    };
    const models = Object.fromEntries(
      Object.entries(tasci.models ?? {}).map(([modelAlias, model]) => [
        modelAlias,
        model && model.endpoint === endpointDraft.originalAlias
          ? { ...model, endpoint: alias }
          : model,
      ]),
    );
    try {
      await persistTasci({ ...tasci, endpoints, models });
      setEndpointDraft(undefined);
    } catch (cause) {
      setActionError(errorMessage(cause));
    }
  };

  const saveModel = async (event: FormEvent) => {
    event.preventDefault();
    if (!modelDraft || mutationDisabled) return;
    const validationError = validateModelDraft(modelDraft, tasci);
    if (validationError) {
      setActionError(validationError);
      return;
    }
    const alias = modelDraft.alias.trim();
    const models = { ...(tasci.models ?? {}) };
    if (modelDraft.originalAlias && modelDraft.originalAlias !== alias) {
      delete models[modelDraft.originalAlias];
    }
    models[alias] = {
      endpoint: modelDraft.endpoint,
      model: modelDraft.model.trim(),
      displayName: optionalTrimmed(modelDraft.displayName),
      contextWindow: positiveInteger(modelDraft.contextWindow) as config.WorkspaceTasciModel["contextWindow"],
      maxOutputTokens: positiveInteger(modelDraft.maxOutputTokens) as config.WorkspaceTasciModel["maxOutputTokens"],
      toolCalls: modelDraft.toolCalls,
      parallelToolCalls: modelDraft.parallelToolCalls,
      pricing: tasciPricing(modelDraft.pricing),
    };
    const defaultModel = tasci.defaultModel === modelDraft.originalAlias
      ? alias
      : tasci.defaultModel;
    try {
      await persistTasci({ ...tasci, defaultModel, models });
      setModelDraft(undefined);
    } catch (cause) {
      setActionError(errorMessage(cause));
    }
  };

  const updateDefaultModel = async (alias: string) => {
    if (mutationDisabled) return;
    try {
      await persistTasci({
        ...tasci,
        defaultModel: alias === NO_DEFAULT_MODEL ? undefined : alias,
      });
    } catch (cause) {
      setActionError(errorMessage(cause));
    }
  };

  const deleteConfiguredItem = async () => {
    if (!deleteTarget || deleting) return;
    setDeleting(true);
    try {
      if (deleteTarget.kind === "model") {
        const models = { ...(tasci.models ?? {}) };
        delete models[deleteTarget.alias];
        await persistTasci({
          ...tasci,
          defaultModel: tasci.defaultModel === deleteTarget.alias
            ? undefined
            : tasci.defaultModel,
          models,
        });
      } else {
        const endpoints = { ...(tasci.endpoints ?? {}) };
        delete endpoints[deleteTarget.alias];
        const models = Object.fromEntries(
          Object.entries(tasci.models ?? {}).filter(([, model]) =>
            model?.endpoint !== deleteTarget.alias
          ),
        );
        await persistTasci({
          ...tasci,
          defaultModel: tasci.defaultModel && models[tasci.defaultModel]
            ? tasci.defaultModel
            : undefined,
          endpoints,
          models,
        });
      }
      setDeleteTarget(undefined);
    } catch (cause) {
      setActionError(errorMessage(cause));
      setDeleteTarget(undefined);
    } finally {
      setDeleting(false);
    }
  };

  const reportedError = actionError
    ?? configState.error?.message
    ?? configState.value?.lastSettingsError?.message;

  return (
    <>
      <div className="max-w-5xl space-y-6">
        <div>
          <h2 className="text-sm font-semibold text-foreground">Tasci</h2>
          <p className="mt-1 max-w-3xl text-xs leading-5 text-subtle">
            Configure protocol-compatible inference endpoints and the model aliases routed through them. Use workspace network secret injection for authenticated endpoints so credentials remain outside the VM.
          </p>
        </div>

        {reportedError ? (
          <p className="rounded-xl border border-red-500/20 bg-red-500/5 px-3 py-2 text-xs text-red-200" role="alert">
            {reportedError}
          </p>
        ) : null}

        <section aria-labelledby="tasci-endpoints-title">
          <SectionHeader
            title="API Endpoints"
            description="Each endpoint declares a wire protocol, base URL, and optional placeholder-bearing authorization header."
            action={(
              <Button
                size="small"
                disabled={mutationDisabled || endpointDraft !== undefined}
                onClick={() => setEndpointDraft(newEndpointDraft())}
              >
                <Plus aria-hidden="true" className="size-3.5" />
                Add endpoint
              </Button>
            )}
            id="tasci-endpoints-title"
          />
          <div className="mt-3 overflow-hidden rounded-xl border border-ui-border">
            {endpointEntries.map(({ alias, endpoint }) => (
              <EndpointRow
                alias={alias}
                disabled={mutationDisabled || endpointDraft !== undefined}
                endpoint={endpoint}
                key={alias}
                onDelete={() => setDeleteTarget({ kind: "endpoint", alias })}
                onEdit={() => setEndpointDraft(editEndpointDraft(alias, endpoint))}
              />
            ))}
            {!endpointEntries.length ? (
              <p className="bg-surface/50 px-4 py-5 text-xs text-subtle">
                No inference endpoints are configured.
              </p>
            ) : null}
          </div>
          {endpointDraft ? (
            <EndpointEditor
              draft={endpointDraft}
              disabled={mutationDisabled}
              onCancel={() => setEndpointDraft(undefined)}
              onChange={setEndpointDraft}
              onSubmit={saveEndpoint}
            />
          ) : null}
        </section>

        <section aria-labelledby="tasci-models-title">
          <SectionHeader
            title="Models"
            description="Model aliases bind provider-native identifiers, capabilities, and optional token prices to an endpoint."
            action={(
              <Button
                size="small"
                disabled={mutationDisabled || modelDraft !== undefined || !endpointEntries.length}
                onClick={() => setModelDraft(newModelDraft(endpointEntries[0]?.alias ?? ""))}
              >
                <Plus aria-hidden="true" className="size-3.5" />
                Add model
              </Button>
            )}
            id="tasci-models-title"
          />
          <div className="mt-3 overflow-hidden rounded-xl border border-ui-border">
            {modelEntries.map(({ alias, model }) => (
              <ModelRow
                alias={alias}
                disabled={mutationDisabled || modelDraft !== undefined}
                isDefault={tasci.defaultModel === alias}
                key={alias}
                model={model}
                onDelete={() => setDeleteTarget({ kind: "model", alias })}
                onEdit={() => setModelDraft(editModelDraft(alias, model))}
              />
            ))}
            {!modelEntries.length ? (
              <p className="bg-surface/50 px-4 py-5 text-xs text-subtle">
                {endpointEntries.length
                  ? "No models are configured."
                  : "Add an API endpoint before configuring models."}
              </p>
            ) : null}
          </div>
          {modelDraft ? (
            <ModelEditor
              draft={modelDraft}
              endpoints={endpointEntries}
              disabled={mutationDisabled}
              onCancel={() => setModelDraft(undefined)}
              onChange={setModelDraft}
              onSubmit={saveModel}
            />
          ) : null}
        </section>

        <section aria-labelledby="tasci-default-model-title">
          <SectionHeader
            title="Default Model"
            description="Used for new Tasci chats unless a model is selected explicitly."
            id="tasci-default-model-title"
          />
          <SelectControl
            className="mt-3 max-w-md"
            disabled={mutationDisabled || !modelEntries.length}
            label="Default Tasci model"
            options={[
              { label: "No default", value: NO_DEFAULT_MODEL },
              ...modelEntries.map(({ alias, model }) => ({
                label: model.displayName || alias,
                value: alias,
              })),
            ]}
            value={tasci.defaultModel ?? NO_DEFAULT_MODEL}
            onChange={(value) => void updateDefaultModel(value)}
          />
        </section>

      </div>

      <ConfirmDialog
        confirmLabel={deleteTarget?.kind === "endpoint" ? "Delete endpoint" : "Delete model"}
        description={deleteDescription(deleteTarget, tasci.models)}
        destructive
        open={deleteTarget !== undefined}
        pending={deleting}
        title={deleteTarget?.kind === "endpoint" ? "Delete API Endpoint?" : "Delete Model?"}
        onOpenChange={(open) => {
          if (!open) setDeleteTarget(undefined);
        }}
        onConfirm={() => void deleteConfiguredItem()}
      />
    </>
  );
}

function EndpointRow({
  alias,
  endpoint,
  disabled,
  onEdit,
  onDelete,
}: {
  alias: string;
  endpoint: config.WorkspaceTasciEndpoint;
  disabled: boolean;
  onEdit: () => void;
  onDelete: () => void;
}) {
  return (
    <div className="grid grid-cols-[minmax(0,1fr)_auto] items-center gap-3 border-b border-ui-border bg-surface/50 px-4 py-3 last:border-b-0">
      <div className="min-w-0">
        <p className="truncate text-xs font-medium text-foreground">{endpoint.displayName || alias}</p>
        <p className="mt-1 truncate font-mono text-[10px] text-subtle">{endpoint.baseUrl}</p>
        <p className="mt-1 text-[10px] text-subtle">
          OpenAI Chat Completions · {endpoint.authorization
            ? `${endpoint.authorization.header} template`
            : "No authentication"}
        </p>
      </div>
      <div className="flex items-center gap-1">
        <Button aria-label={`Edit ${alias}`} className="size-8 p-0" size="icon" disabled={disabled} onClick={onEdit}>
          <Pencil aria-hidden="true" className="size-3.5" />
        </Button>
        <Button aria-label={`Delete ${alias}`} className="size-8 p-0" size="icon" disabled={disabled} onClick={onDelete}>
          <Trash2 aria-hidden="true" className="size-3.5" />
        </Button>
      </div>
    </div>
  );
}

function ModelRow({
  alias,
  model,
  isDefault,
  disabled,
  onEdit,
  onDelete,
}: {
  alias: string;
  model: config.WorkspaceTasciModel;
  isDefault: boolean;
  disabled: boolean;
  onEdit: () => void;
  onDelete: () => void;
}) {
  return (
    <div className="grid grid-cols-[minmax(0,1fr)_auto] items-center gap-3 border-b border-ui-border bg-surface/50 px-4 py-3 last:border-b-0">
      <div className="min-w-0">
        <div className="flex flex-wrap items-center gap-2">
          <p className="truncate text-xs font-medium text-foreground">{model.displayName || alias}</p>
          {isDefault ? <span className="rounded bg-accent/15 px-1.5 py-0.5 text-[9px] font-medium text-accent-text">Default</span> : null}
        </div>
        <p className="mt-1 truncate font-mono text-[10px] text-subtle">{alias} → {model.endpoint}/{model.model}</p>
        <p className="mt-1 text-[10px] text-subtle">
          {model.toolCalls === false ? "No tool calls" : "Tool calls"}
          {model.parallelToolCalls ? " · Parallel calls" : ""}
          {model.contextWindow ? ` · ${model.contextWindow} context tokens` : ""}
          {model.pricing ? ` · Pricing ${model.pricing.catalogVersion}` : ""}
        </p>
      </div>
      <div className="flex items-center gap-1">
        <Button aria-label={`Edit ${alias}`} className="size-8 p-0" size="icon" disabled={disabled} onClick={onEdit}>
          <Pencil aria-hidden="true" className="size-3.5" />
        </Button>
        <Button aria-label={`Delete ${alias}`} className="size-8 p-0" size="icon" disabled={disabled} onClick={onDelete}>
          <Trash2 aria-hidden="true" className="size-3.5" />
        </Button>
      </div>
    </div>
  );
}

function EndpointEditor({
  draft,
  disabled,
  onChange,
  onCancel,
  onSubmit,
}: {
  draft: EndpointDraft;
  disabled: boolean;
  onChange: (draft: EndpointDraft) => void;
  onCancel: () => void;
  onSubmit: (event: FormEvent) => void;
}) {
  return (
    <form className="mt-3 rounded-xl border border-ui-border bg-surface/60 p-4" onSubmit={onSubmit}>
      <h4 className="text-xs font-semibold text-foreground">{draft.originalAlias ? "Edit API Endpoint" : "Add API Endpoint"}</h4>
      <div className="mt-4 grid gap-3 sm:grid-cols-2">
        <SettingsField label="Alias">
          <TextInput autoFocus className="w-full" required value={draft.alias} onChange={(event) => onChange({ ...draft, alias: event.target.value })} />
        </SettingsField>
        <SettingsField label="Display name">
          <TextInput className="w-full" placeholder="Optional" value={draft.displayName} onChange={(event) => onChange({ ...draft, displayName: event.target.value })} />
        </SettingsField>
        <div className="sm:col-span-2">
          <SelectControl
            disabled
            label="Protocol"
            options={[{ label: "OpenAI Chat Completions", value: "OpenAiChatCompletions" }]}
            value="OpenAiChatCompletions"
            onChange={() => undefined}
          />
        </div>
        <div className="sm:col-span-2">
          <SettingsField label="API base URL">
            <TextInput className="w-full font-mono" placeholder="https://api.example.com/v1" required type="url" value={draft.baseUrl} onChange={(event) => onChange({ ...draft, baseUrl: event.target.value })} />
          </SettingsField>
        </div>
      </div>

      <label className="mt-4 flex items-center gap-2 text-xs text-muted">
        <input
          checked={draft.authenticated}
          className="size-3.5 accent-accent"
          type="checkbox"
          onChange={(event) => onChange({ ...draft, authenticated: event.target.checked })}
        />
        Send an authorization header template
      </label>

      {draft.authenticated ? (
        <div className="mt-3 grid gap-3 rounded-lg border border-ui-border/70 p-3 sm:grid-cols-2">
          <SettingsField label="Header">
            <TextInput className="w-full font-mono" required value={draft.header} onChange={(event) => onChange({ ...draft, header: event.target.value })} />
          </SettingsField>
          <div>
            <SettingsField label="Value template">
              <TextInput className="w-full font-mono" placeholder="Bearer tascarrel-secret:api-token" required value={draft.value} onChange={(event) => onChange({ ...draft, value: event.target.value })} />
            </SettingsField>
            <p className="mt-1 text-[10px] text-subtle">
              Store only a placeholder here. Configure its replacement under network secret injection; never enter the credential itself.
            </p>
          </div>
        </div>
      ) : null}

      <EditorActions disabled={disabled} onCancel={onCancel} />
    </form>
  );
}

function ModelEditor({
  draft,
  endpoints,
  disabled,
  onChange,
  onCancel,
  onSubmit,
}: {
  draft: ModelDraft;
  endpoints: EndpointEntry[];
  disabled: boolean;
  onChange: (draft: ModelDraft) => void;
  onCancel: () => void;
  onSubmit: (event: FormEvent) => void;
}) {
  return (
    <form className="mt-3 rounded-xl border border-ui-border bg-surface/60 p-4" onSubmit={onSubmit}>
      <h4 className="text-xs font-semibold text-foreground">{draft.originalAlias ? "Edit Model" : "Add Model"}</h4>
      <div className="mt-4 grid gap-3 sm:grid-cols-2">
        <SettingsField label="Alias">
          <TextInput autoFocus className="w-full" required value={draft.alias} onChange={(event) => onChange({ ...draft, alias: event.target.value })} />
        </SettingsField>
        <SettingsField label="Display name">
          <TextInput className="w-full" placeholder="Optional" value={draft.displayName} onChange={(event) => onChange({ ...draft, displayName: event.target.value })} />
        </SettingsField>
        <SelectControl
          label="API endpoint"
          options={endpoints.map(({ alias, endpoint }) => ({
            label: endpoint.displayName || alias,
            value: alias,
          }))}
          required
          value={draft.endpoint}
          onChange={(endpoint) => onChange({ ...draft, endpoint })}
        />
        <SettingsField label="Provider model identifier">
          <TextInput className="w-full font-mono" required value={draft.model} onChange={(event) => onChange({ ...draft, model: event.target.value })} />
        </SettingsField>
        <SettingsField label="Context window">
          <TextInput className="w-full" min="1" placeholder="Optional" type="number" value={draft.contextWindow} onChange={(event) => onChange({ ...draft, contextWindow: event.target.value })} />
        </SettingsField>
        <SettingsField label="Maximum output tokens">
          <TextInput className="w-full" min="1" placeholder="Optional" type="number" value={draft.maxOutputTokens} onChange={(event) => onChange({ ...draft, maxOutputTokens: event.target.value })} />
        </SettingsField>
      </div>
      <div className="mt-4 flex flex-wrap gap-x-6 gap-y-2">
        <label className="flex items-center gap-2 text-xs text-muted">
          <input
            checked={draft.toolCalls}
            className="size-3.5 accent-accent"
            type="checkbox"
            onChange={(event) => onChange({
              ...draft,
              toolCalls: event.target.checked,
              parallelToolCalls: event.target.checked && draft.parallelToolCalls,
            })}
          />
          Structured tool calls
        </label>
        <label className="flex items-center gap-2 text-xs text-muted">
          <input
            checked={draft.parallelToolCalls}
            className="size-3.5 accent-accent"
            disabled={!draft.toolCalls}
            type="checkbox"
            onChange={(event) => onChange({ ...draft, parallelToolCalls: event.target.checked })}
          />
          Parallel tool calls
        </label>
      </div>
      <div className="mt-4 border-t border-ui-border/70 pt-4">
        <label className="flex items-center gap-2 text-xs text-muted">
          <input
            checked={draft.pricing.enabled}
            className="size-3.5 accent-accent"
            type="checkbox"
            onChange={(event) => onChange({
              ...draft,
              pricing: { ...draft.pricing, enabled: event.target.checked },
            })}
          />
          Associate token prices with this model
        </label>
        {draft.pricing.enabled ? (
          <TasciPricingEditor
            draft={draft.pricing}
            onChange={(pricing) => onChange({ ...draft, pricing })}
          />
        ) : null}
      </div>
      <EditorActions disabled={disabled} onCancel={onCancel} />
    </form>
  );
}

function EditorActions({ disabled, onCancel }: { disabled: boolean; onCancel: () => void }) {
  return (
    <div className="mt-4 flex justify-end gap-2">
      <Button disabled={disabled} onClick={onCancel}>
        <X aria-hidden="true" className="size-3.5" />
        Cancel
      </Button>
      <Button disabled={disabled} type="submit" variant="primary">
        <Save aria-hidden="true" className="size-3.5" />
        Save
      </Button>
    </div>
  );
}

function SectionHeader({
  id,
  title,
  description,
  action,
}: {
  id: string;
  title: string;
  description: string;
  action?: React.ReactNode;
}) {
  return (
    <div className="flex flex-wrap items-end justify-between gap-3">
      <div>
        <h3 className="text-xs font-medium text-foreground" id={id}>{title}</h3>
        <p className="mt-1 max-w-3xl text-[11px] leading-5 text-subtle">{description}</p>
      </div>
      {action}
    </div>
  );
}

function sortedEndpointEntries(
  endpoints: config.WorkspaceTasciSettings["endpoints"],
): EndpointEntry[] {
  return Object.entries(endpoints ?? {})
    .flatMap(([alias, endpoint]) => endpoint ? [{ alias, endpoint }] : [])
    .toSorted((left, right) => left.alias.localeCompare(right.alias));
}

function sortedModelEntries(
  models: config.WorkspaceTasciSettings["models"],
): ModelEntry[] {
  return Object.entries(models ?? {})
    .flatMap(([alias, model]) => model ? [{ alias, model }] : [])
    .toSorted((left, right) => left.alias.localeCompare(right.alias));
}

function newEndpointDraft(): EndpointDraft {
  return {
    alias: "",
    displayName: "",
    baseUrl: "",
    authenticated: false,
    header: "Authorization",
    value: "Bearer tascarrel-secret:api-token",
  };
}

function editEndpointDraft(
  alias: string,
  endpoint: config.WorkspaceTasciEndpoint,
): EndpointDraft {
  return {
    originalAlias: alias,
    alias,
    displayName: endpoint.displayName ?? "",
    baseUrl: endpoint.baseUrl,
    authenticated: endpoint.authorization !== undefined,
    header: endpoint.authorization?.header ?? "Authorization",
    value: authorizationValue(endpoint.authorization),
  };
}

function newModelDraft(endpoint: string): ModelDraft {
  return {
    alias: "",
    displayName: "",
    endpoint,
    model: "",
    contextWindow: "",
    maxOutputTokens: "",
    toolCalls: true,
    parallelToolCalls: true,
    pricing: tasciPricingDraft(),
  };
}

function editModelDraft(alias: string, model: config.WorkspaceTasciModel): ModelDraft {
  return {
    originalAlias: alias,
    alias,
    displayName: model.displayName ?? "",
    endpoint: model.endpoint,
    model: model.model,
    contextWindow: model.contextWindow?.toString() ?? "",
    maxOutputTokens: model.maxOutputTokens?.toString() ?? "",
    toolCalls: model.toolCalls !== false,
    parallelToolCalls: model.parallelToolCalls === true,
    pricing: tasciPricingDraft(model.pricing),
  };
}

function validateEndpointDraft(
  draft: EndpointDraft,
  endpoints: config.WorkspaceTasciSettings["endpoints"],
): string | undefined {
  const alias = draft.alias.trim();
  if (!alias) return "Endpoint alias is required.";
  if (alias !== draft.alias) return "Endpoint alias cannot contain surrounding whitespace.";
  if (alias !== draft.originalAlias && endpoints?.[alias]) return "Endpoint alias is already configured.";
  try {
    const url = new URL(draft.baseUrl);
    if (!["http:", "https:"].includes(url.protocol)
      || url.username
      || url.password
      || url.search
      || url.hash) {
      return "API base URL must be an HTTP or HTTPS URL without credentials, a query, or a fragment.";
    }
  } catch {
    return "API base URL is invalid.";
  }
  if (!draft.authenticated) return undefined;
  if (!draft.header.trim()) return "Authorization header is required.";
  if (!draft.value) return "Authorization value template is required.";
  return undefined;
}

function validateModelDraft(
  draft: ModelDraft,
  tasci: config.WorkspaceTasciSettings,
): string | undefined {
  const alias = draft.alias.trim();
  if (!alias) return "Model alias is required.";
  if (alias !== draft.alias) return "Model alias cannot contain surrounding whitespace.";
  if (alias !== draft.originalAlias && tasci.models?.[alias]) return "Model alias is already configured.";
  if (!tasci.endpoints?.[draft.endpoint]) return "Select a configured API endpoint.";
  if (!draft.model.trim()) return "Provider model identifier is required.";
  if (draft.model.trim() !== draft.model) return "Provider model identifier cannot contain surrounding whitespace.";
  if (draft.contextWindow && !positiveInteger(draft.contextWindow)) return "Context window must be a positive integer.";
  if (draft.maxOutputTokens && !positiveInteger(draft.maxOutputTokens)) return "Maximum output tokens must be a positive integer.";
  return validateTasciPricingDraft(draft.pricing);
}

function authorizationValue(
  authorization: config.WorkspaceTasciAuthorization | undefined,
): string {
  if (!authorization) return "Bearer tascarrel-secret:api-token";
  if (authorization.value !== undefined) return authorization.value;
  const secret = authorization.credential?.secret;
  if (!secret) return authorization.prefix ?? "";
  const placeholder = `tascarrel-secret:${secret.toLowerCase().replaceAll("_", "-")}`;
  return `${authorization.prefix ?? ""}${placeholder}`;
}

function positiveInteger(value: string): number | undefined {
  if (!value) return undefined;
  const number = Number(value);
  return Number.isSafeInteger(number) && number > 0 ? number : undefined;
}

function optionalTrimmed(value: string): string | undefined {
  const trimmed = value.trim();
  return trimmed || undefined;
}

function deleteDescription(
  target: DeleteTarget | undefined,
  models: config.WorkspaceTasciSettings["models"],
): string {
  if (!target) return "Delete this Tasci configuration?";
  if (target.kind === "model") {
    return `Delete model ${target.alias}? The default selection is cleared if it uses this model.`;
  }
  const dependentModels = Object.values(models ?? {}).filter((model) =>
    model?.endpoint === target.alias
  ).length;
  return dependentModels
    ? `Delete endpoint ${target.alias} and its ${dependentModels} configured ${dependentModels === 1 ? "model" : "models"}?`
    : `Delete endpoint ${target.alias}?`;
}

function errorMessage(cause: unknown): string {
  return cause instanceof Error ? cause.message : String(cause);
}
