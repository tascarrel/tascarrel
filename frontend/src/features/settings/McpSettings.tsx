import { Pencil, Plus, Save, Trash2, X } from "lucide-react";
import { useEffect, useState, type FormEvent } from "react";

import { hostApi } from "../../api/client.ts";
import type {
  chats,
  config,
  workspaces,
} from "../../api/generated/index.ts";
import { Button } from "../../components/ui/Button.tsx";
import { ConfirmDialog } from "../../components/ui/ConfirmDialog.tsx";
import { TextInput } from "../../components/ui/TextInput.tsx";
import { useWorkspaceConfig } from "../workspaces/runtimeState.ts";
import { sameWorkspaceSettings } from "./settingsComparison.ts";
import { SettingsField } from "./SettingsField.tsx";

const HARNESS_OPTIONS: {
  value: chats.ChatHarnessKind;
  label: string;
}[] = [
  { value: "Tasci", label: "Tasci" },
  { value: "Codex", label: "Codex" },
  { value: "ClaudeCode", label: "Claude Code" },
];

type McpServerDraft = {
  originalName?: string;
  name: string;
  displayName: string;
  endpoint: string;
  headers: McpHeaderDraft[];
  harnesses: chats.ChatHarnessKind[];
};

type McpHeaderDraft = {
  id: number;
  name: string;
  value: string;
};

type McpServerEntry = {
  name: string;
  server: config.WorkspaceMcpServer;
};

let nextHeaderDraftId = 0;

export function McpSettings({
  workspace,
}: {
  workspace: workspaces.WorkspaceName;
}) {
  const configState = useWorkspaceConfig(workspace);
  const [pendingSettings, setPendingSettings] =
    useState<config.WorkspaceSettings>();
  const [savingSettings, setSavingSettings] = useState(false);
  const [draft, setDraft] = useState<McpServerDraft>();
  const [deleteTarget, setDeleteTarget] = useState<string>();
  const [deleting, setDeleting] = useState(false);
  const [error, setError] = useState<string>();
  const observedSettings = configState.value?.settings;
  const configInstanceId = configState.value?.configInstanceId;
  const settings = pendingSettings ?? observedSettings ?? {};
  const servers = settings.chat?.mcpServers;
  const disabled = savingSettings
    || pendingSettings !== undefined
    || !configInstanceId
    || Boolean(configState.value?.lastSettingsError);
  const loading = !configInstanceId
    && !configState.error
    && !configState.value?.lastSettingsError;
  const statusMessage = pendingSettings !== undefined
    ? "Saving MCP settings…"
    : loading
      ? "Loading MCP settings…"
      : undefined;
  const entries = sortedMcpServerEntries(servers);

  useEffect(() => {
    if (
      pendingSettings !== undefined
      && sameWorkspaceSettings(configState.value?.settings, pendingSettings)
    ) setPendingSettings(undefined);
  }, [configState.value?.settings, pendingSettings]);

  useEffect(() => {
    setDraft(undefined);
    setDeleteTarget(undefined);
    setError(undefined);
  }, [workspace]);

  const persistServers = async (
    nextServers: NonNullable<config.WorkspaceChatSettings["mcpServers"]>,
  ) => {
    if (configState.value?.lastSettingsError) {
      throw new Error("Fix settings.json before changing MCP settings in the UI.");
    }
    if (!configInstanceId) {
      throw new Error("Workspace configuration is not ready yet.");
    }
    const nextSettings: config.WorkspaceSettings = {
      ...settings,
      chat: {
        ...(settings.chat ?? {}),
        mcpServers: nextServers,
      },
    };
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

  const save = async (event: FormEvent) => {
    event.preventDefault();
    if (!draft || disabled) return;
    const validationError = validateMcpServerDraft(draft, servers);
    if (validationError) {
      setError(validationError);
      return;
    }
    const nextServers = { ...(servers ?? {}) };
    if (draft.originalName && draft.originalName !== draft.name) {
      delete nextServers[draft.originalName];
    }
    const headers = Object.fromEntries(
      draft.headers.map((header) => [header.name, header.value]),
    );
    nextServers[draft.name] = {
      displayName: optionalTrimmed(draft.displayName),
      endpoint: draft.endpoint,
      headers: Object.keys(headers).length ? headers : undefined,
      harnesses: draft.harnesses.length === HARNESS_OPTIONS.length
        ? undefined
        : draft.harnesses,
    };
    setError(undefined);
    try {
      await persistServers(nextServers);
      setDraft(undefined);
    } catch (cause) {
      setError(errorMessage(cause));
    }
  };

  const deleteServer = async () => {
    if (!deleteTarget || deleting) return;
    const nextServers = { ...(servers ?? {}) };
    delete nextServers[deleteTarget];
    setDeleting(true);
    setError(undefined);
    try {
      await persistServers(nextServers);
      setDeleteTarget(undefined);
    } catch (cause) {
      setError(errorMessage(cause));
      setDeleteTarget(undefined);
    } finally {
      setDeleting(false);
    }
  };

  return (
    <>
      <div className="max-w-4xl">
        <div className="mb-5">
          <h2 className="text-sm font-semibold text-foreground">MCP Servers</h2>
          <p className="mt-1 max-w-3xl text-xs leading-5 text-subtle">
            Configure Streamable HTTP servers once, then choose which coding
            harnesses receive them for new sessions. Every tool advertised by a
            configured server is trusted.
          </p>
        </div>

        {error || configState.error || configState.value?.lastSettingsError ? (
          <p
            className="mb-4 rounded-xl border border-red-500/20 bg-red-500/5 px-3 py-2 text-xs text-red-200"
            role="alert"
          >
            {error
              ?? configState.error?.message
              ?? configState.value?.lastSettingsError?.message}
          </p>
        ) : null}

        {statusMessage ? (
          <p className="mb-4 text-xs text-subtle" role="status">
            {statusMessage}
          </p>
        ) : null}

        <section aria-labelledby="mcp-server-catalog-title">
          <div className="flex flex-wrap items-end justify-between gap-3">
            <div>
              <h3
                className="text-xs font-medium text-foreground"
                id="mcp-server-catalog-title"
              >
                Server Catalog
              </h3>
              <p className="mt-1 max-w-3xl text-[11px] leading-5 text-subtle">
                Header values may contain host-side secret-injection
                placeholders. Existing harness sessions keep the catalog with
                which they started.
              </p>
            </div>
            <Button
              disabled={disabled || draft !== undefined}
              size="small"
              onClick={() => {
                setDraft(newMcpServerDraft());
                setError(undefined);
              }}
            >
              <Plus aria-hidden="true" className="size-3.5" />
              Add server
            </Button>
          </div>

          <div className="mt-3 overflow-hidden rounded-xl border border-ui-border">
            {entries.map(({ name, server }) => (
              <McpServerRow
                disabled={disabled || draft !== undefined}
                key={name}
                name={name}
                server={server}
                onDelete={() => setDeleteTarget(name)}
                onEdit={() => {
                  setDraft(editMcpServerDraft(name, server));
                  setError(undefined);
                }}
              />
            ))}
            {!entries.length && configInstanceId ? (
              <p className="bg-surface/50 px-4 py-5 text-xs text-subtle">
                No MCP servers are configured.
              </p>
            ) : null}
          </div>

          {draft ? (
            <McpServerEditor
              disabled={disabled}
              draft={draft}
              onCancel={() => {
                setDraft(undefined);
                setError(undefined);
              }}
              onChange={setDraft}
              onSubmit={save}
            />
          ) : null}
        </section>
      </div>

      <ConfirmDialog
        confirmLabel="Delete server"
        description={`Delete MCP server ${deleteTarget ?? ""}? Its tools will no longer be available to new harness sessions.`}
        destructive
        open={deleteTarget !== undefined}
        pending={deleting}
        title="Delete MCP Server?"
        onOpenChange={(open) => {
          if (!open) setDeleteTarget(undefined);
        }}
        onConfirm={() => void deleteServer()}
      />
    </>
  );
}

function McpServerRow({
  name,
  server,
  disabled,
  onEdit,
  onDelete,
}: {
  name: string;
  server: config.WorkspaceMcpServer;
  disabled: boolean;
  onEdit: () => void;
  onDelete: () => void;
}) {
  const headerCount = Object.keys(server.headers ?? {}).length;
  return (
    <div className="grid grid-cols-[minmax(0,1fr)_auto] items-center gap-3 border-b border-ui-border bg-surface/50 px-4 py-3 last:border-b-0">
      <div className="min-w-0">
        <p className="truncate text-xs font-medium text-foreground">
          {server.displayName || name}
        </p>
        <p className="mt-1 truncate font-mono text-[10px] text-subtle">
          {server.endpoint}
        </p>
        <p className="mt-1 text-[10px] text-subtle">
          Server key <code>{name}</code> ·{" "}
          {headerCount
            ? `${headerCount} header ${headerCount === 1 ? "template" : "templates"}`
            : "No custom headers"}{" "}
          · {mcpHarnessLabel(server.harnesses)}
        </p>
      </div>
      <div className="flex items-center gap-1">
        <Button
          aria-label={`Edit ${name}`}
          className="size-8 p-0"
          disabled={disabled}
          size="icon"
          onClick={onEdit}
        >
          <Pencil aria-hidden="true" className="size-3.5" />
        </Button>
        <Button
          aria-label={`Delete ${name}`}
          className="size-8 p-0"
          disabled={disabled}
          size="icon"
          onClick={onDelete}
        >
          <Trash2 aria-hidden="true" className="size-3.5" />
        </Button>
      </div>
    </div>
  );
}

function McpServerEditor({
  draft,
  disabled,
  onChange,
  onCancel,
  onSubmit,
}: {
  draft: McpServerDraft;
  disabled: boolean;
  onChange: (draft: McpServerDraft) => void;
  onCancel: () => void;
  onSubmit: (event: FormEvent) => void;
}) {
  return (
    <form
      className="mt-3 rounded-xl border border-ui-border bg-surface/60 p-4"
      onSubmit={onSubmit}
    >
      <h4 className="text-xs font-semibold text-foreground">
        {draft.originalName ? "Edit MCP Server" : "Add MCP Server"}
      </h4>
      <div className="mt-4 grid gap-3 sm:grid-cols-2">
        <SettingsField label="Name">
          <TextInput
            autoFocus
            className="w-full font-mono"
            required
            value={draft.name}
            onChange={(event) =>
              onChange({ ...draft, name: event.target.value })
            }
          />
        </SettingsField>
        <SettingsField label="Display name">
          <TextInput
            className="w-full"
            placeholder="Optional"
            value={draft.displayName}
            onChange={(event) =>
              onChange({ ...draft, displayName: event.target.value })
            }
          />
        </SettingsField>
        <div className="sm:col-span-2">
          <SettingsField label="Streamable HTTP endpoint">
            <TextInput
              className="w-full font-mono"
              placeholder="https://mcp.example.com/mcp"
              required
              type="url"
              value={draft.endpoint}
              onChange={(event) =>
                onChange({ ...draft, endpoint: event.target.value })
              }
            />
          </SettingsField>
        </div>
      </div>

      <fieldset className="mt-4">
        <legend className="text-[11px] font-medium text-foreground">
          Harnesses
        </legend>
        <p className="mt-1 text-[10px] leading-4 text-subtle">
          Select every harness that should receive this server when starting a
          new session.
        </p>
        <div className="mt-2 flex flex-wrap gap-x-5 gap-y-2">
          {HARNESS_OPTIONS.map((option) => (
            <label
              className="flex items-center gap-2 text-xs text-foreground"
              key={option.value}
            >
              <input
                checked={draft.harnesses.includes(option.value)}
                className="size-3.5 accent-accent"
                disabled={disabled}
                type="checkbox"
                onChange={(event) =>
                  onChange({
                    ...draft,
                    harnesses: event.target.checked
                      ? [...draft.harnesses, option.value]
                      : draft.harnesses.filter(
                          (harness) => harness !== option.value,
                        ),
                  })
                }
              />
              {option.label}
            </label>
          ))}
        </div>
      </fieldset>

      <div className="mt-4 flex flex-wrap items-end justify-between gap-3">
        <div>
          <h5 className="text-[11px] font-medium text-foreground">
            HTTP Headers
          </h5>
          <p className="mt-1 text-[10px] leading-4 text-subtle">
            Store placeholders, never credentials. Network secret injection
            replaces matching values after requests leave the workspace.
          </p>
        </div>
        <Button
          disabled={disabled}
          size="small"
          onClick={() =>
            onChange({
              ...draft,
              headers: [...draft.headers, newMcpHeaderDraft()],
            })
          }
        >
          <Plus aria-hidden="true" className="size-3.5" />
          Add header
        </Button>
      </div>

      {draft.headers.length ? (
        <div className="mt-3 space-y-2">
          {draft.headers.map((header) => (
            <div
              className="grid gap-2 sm:grid-cols-[minmax(0,1fr)_minmax(0,2fr)_auto]"
              key={header.id}
            >
              <TextInput
                aria-label="Header name"
                className="w-full font-mono"
                placeholder="Authorization"
                required
                value={header.name}
                onChange={(event) =>
                  onChange({
                    ...draft,
                    headers: updateHeader(draft.headers, header.id, {
                      ...header,
                      name: event.target.value,
                    }),
                  })
                }
              />
              <TextInput
                aria-label="Header value template"
                className="w-full font-mono"
                placeholder="Bearer tascarrel-secret:mcp-token"
                value={header.value}
                onChange={(event) =>
                  onChange({
                    ...draft,
                    headers: updateHeader(draft.headers, header.id, {
                      ...header,
                      value: event.target.value,
                    }),
                  })
                }
              />
              <Button
                aria-label={`Remove ${header.name || "header"}`}
                className="size-8 p-0"
                disabled={disabled}
                size="icon"
                onClick={() =>
                  onChange({
                    ...draft,
                    headers: draft.headers.filter(
                      (candidate) => candidate.id !== header.id,
                    ),
                  })
                }
              >
                <Trash2 aria-hidden="true" className="size-3.5" />
              </Button>
            </div>
          ))}
        </div>
      ) : (
        <p className="mt-3 text-[10px] text-subtle">
          No custom headers are sent.
        </p>
      )}

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
    </form>
  );
}

function sortedMcpServerEntries(
  servers: config.WorkspaceChatSettings["mcpServers"],
): McpServerEntry[] {
  return Object.entries(servers ?? {})
    .flatMap(([name, server]) => (server ? [{ name, server }] : []))
    .toSorted((left, right) => left.name.localeCompare(right.name));
}

function newMcpServerDraft(): McpServerDraft {
  return {
    name: "",
    displayName: "",
    endpoint: "",
    headers: [],
    harnesses: HARNESS_OPTIONS.map((option) => option.value),
  };
}

function editMcpServerDraft(
  name: string,
  server: config.WorkspaceMcpServer,
): McpServerDraft {
  return {
    originalName: name,
    name,
    displayName: server.displayName ?? "",
    endpoint: server.endpoint,
    headers: Object.entries(server.headers ?? {}).map(([headerName, value]) =>
      newMcpHeaderDraft(headerName, value),
    ),
    harnesses: server.harnesses
      ? [...server.harnesses]
      : HARNESS_OPTIONS.map((option) => option.value),
  };
}

function newMcpHeaderDraft(name = "", value = ""): McpHeaderDraft {
  nextHeaderDraftId += 1;
  return { id: nextHeaderDraftId, name, value };
}

function updateHeader(
  headers: McpHeaderDraft[],
  id: number,
  replacement: McpHeaderDraft,
): McpHeaderDraft[] {
  return headers.map((header) => (header.id === id ? replacement : header));
}

function validateMcpServerDraft(
  draft: McpServerDraft,
  servers: config.WorkspaceChatSettings["mcpServers"],
): string | undefined {
  if (!/^[A-Za-z0-9_-]+$/.test(draft.name)) {
    return "Server name may contain only ASCII letters, digits, hyphens, and underscores.";
  }
  if (draft.name !== draft.originalName && servers?.[draft.name]) {
    return "Server name is already configured.";
  }
  try {
    const url = new URL(draft.endpoint);
    if (
      !["http:", "https:"].includes(url.protocol) ||
      url.username ||
      url.password ||
      draft.endpoint.includes("?") ||
      draft.endpoint.includes("#")
    ) {
      return "MCP endpoint must be an HTTP or HTTPS URL without credentials, a query, or a fragment.";
    }
  } catch {
    return "MCP endpoint is invalid.";
  }
  if (!draft.harnesses.length) {
    return "Select at least one harness.";
  }
  const names = new Set<string>();
  for (const header of draft.headers) {
    if (!header.name || header.name.trim() !== header.name) {
      return "HTTP header names cannot be empty or contain surrounding whitespace.";
    }
    try {
      new Headers([[header.name, header.value]]);
    } catch {
      return "An HTTP header name or value is invalid.";
    }
    const canonicalName = header.name.toLowerCase();
    if (names.has(canonicalName)) {
      return "HTTP header names cannot be repeated without regard to case.";
    }
    names.add(canonicalName);
  }
  return undefined;
}

function mcpHarnessLabel(
  harnesses: config.WorkspaceMcpServer["harnesses"],
): string {
  if (!harnesses || harnesses.length === HARNESS_OPTIONS.length) {
    return "All harnesses";
  }
  return harnesses
    .map(
      (harness) =>
        HARNESS_OPTIONS.find((option) => option.value === harness)?.label
        ?? harness,
    )
    .join(", ");
}

function optionalTrimmed(value: string): string | undefined {
  const trimmed = value.trim();
  return trimmed || undefined;
}

function errorMessage(cause: unknown): string {
  return cause instanceof Error ? cause.message : String(cause);
}
