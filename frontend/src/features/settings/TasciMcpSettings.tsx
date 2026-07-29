import { Pencil, Plus, Save, Trash2, X } from "lucide-react";
import { useState, type FormEvent } from "react";

import type { config } from "../../api/generated/index.ts";
import { Button } from "../../components/ui/Button.tsx";
import { ConfirmDialog } from "../../components/ui/ConfirmDialog.tsx";
import { TextInput } from "../../components/ui/TextInput.tsx";
import { SettingsField } from "./SettingsField.tsx";

type McpServerDraft = {
  originalName?: string;
  name: string;
  displayName: string;
  endpoint: string;
  headers: McpHeaderDraft[];
};

type McpHeaderDraft = {
  id: number;
  name: string;
  value: string;
};

type McpServerEntry = {
  name: string;
  server: config.WorkspaceTasciMcpServer;
};

let nextHeaderDraftId = 0;

export function TasciMcpSettings({
  servers,
  disabled,
  onSave,
}: {
  servers: config.WorkspaceTasciSettings["mcpServers"];
  disabled: boolean;
  onSave: (
    servers: NonNullable<config.WorkspaceTasciSettings["mcpServers"]>,
  ) => Promise<void>;
}) {
  const [draft, setDraft] = useState<McpServerDraft>();
  const [deleteTarget, setDeleteTarget] = useState<string>();
  const [deleting, setDeleting] = useState(false);
  const [error, setError] = useState<string>();
  const entries = sortedMcpServerEntries(servers);

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
    };
    setError(undefined);
    try {
      await onSave(nextServers);
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
      await onSave(nextServers);
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
      <section aria-labelledby="tasci-mcp-servers-title">
        <div className="flex flex-wrap items-end justify-between gap-3">
          <div>
            <h3
              className="text-xs font-medium text-foreground"
              id="tasci-mcp-servers-title"
            >
              MCP Servers
            </h3>
            <p className="mt-1 max-w-3xl text-[11px] leading-5 text-subtle">
              Tasci connects to each Streamable HTTP endpoint and exposes every
              advertised tool. Header values may contain host-side
              secret-injection placeholders.
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

        {error ? (
          <p
            className="mt-3 rounded-xl border border-red-500/20 bg-red-500/5 px-3 py-2 text-xs text-red-200"
            role="alert"
          >
            {error}
          </p>
        ) : null}

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
          {!entries.length ? (
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

      <ConfirmDialog
        confirmLabel="Delete server"
        description={`Delete MCP server ${deleteTarget ?? ""}? Its tools will no longer be available to new Tasci sessions.`}
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
  server: config.WorkspaceTasciMcpServer;
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
          Tool namespace <code>mcp__{name}__*</code> ·{" "}
          {headerCount
            ? `${headerCount} header ${headerCount === 1 ? "template" : "templates"}`
            : "No custom headers"}
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
  servers: config.WorkspaceTasciSettings["mcpServers"],
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
  };
}

function editMcpServerDraft(
  name: string,
  server: config.WorkspaceTasciMcpServer,
): McpServerDraft {
  return {
    originalName: name,
    name,
    displayName: server.displayName ?? "",
    endpoint: server.endpoint,
    headers: Object.entries(server.headers ?? {}).map(([headerName, value]) =>
      newMcpHeaderDraft(headerName, value),
    ),
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
  servers: config.WorkspaceTasciSettings["mcpServers"],
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

function optionalTrimmed(value: string): string | undefined {
  const trimmed = value.trim();
  return trimmed || undefined;
}

function errorMessage(cause: unknown): string {
  return cause instanceof Error ? cause.message : String(cause);
}
