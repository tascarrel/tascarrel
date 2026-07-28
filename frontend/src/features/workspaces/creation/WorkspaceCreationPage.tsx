import { Tabs } from "@base-ui/react/tabs";
import { ChevronDown, LoaderCircle, Plus, Trash2 } from "lucide-react";
import { useMemo, useRef, useState, type FormEvent, type ReactNode } from "react";

import type { workspaces } from "../../../api/generated/index.ts";
import { Badge } from "../../../components/ui/Badge.tsx";
import { Button } from "../../../components/ui/Button.tsx";
import { TascarrelLogo } from "../../../components/ui/TascarrelLogo.tsx";
import { TextInput } from "../../../components/ui/TextInput.tsx";
import {
  createWorkspaceDefinition,
  defaultWorkspaceCreationDraft,
  DEVELOPER_SERVICE_OPTIONS,
  DEVELOPER_TOOL_OPTIONS,
  FEATURE_OPTIONS,
  STACK_OPTIONS,
  type DeveloperServiceId,
  type DeveloperServiceOption,
  type DeveloperToolId,
  type NetworkMode,
  type StackId,
  type WorkspaceCreationDraft,
  type WorkspaceCreationDefinition,
  type WorkspaceFeatureId,
  type WorkspaceRepositoryDraft,
} from "./model.ts";

export function WorkspaceCreationPage({
  canCancel,
  onCancel,
  onCreateWorkspace,
}: {
  canCancel: boolean;
  onCancel: () => void;
  onCreateWorkspace: (input: workspaces.CreateWorkspaceAction) => Promise<void>;
}) {
  const [draft, setDraft] = useState(defaultWorkspaceCreationDraft);
  const [resourceSettingsOpen, setResourceSettingsOpen] = useState(false);
  const [submitted, setSubmitted] = useState(false);
  const [creating, setCreating] = useState(false);
  const [creationError, setCreationError] = useState<string>();
  const nextRepositoryId = useRef(1);
  const result = useMemo(() => createWorkspaceDefinition(draft), [draft]);
  const preview = useMemo(
    () => createWorkspaceDefinition({ ...draft, name: draft.name || "workspace" }).definition,
    [draft],
  );

  const submit = async (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    if (creating) return;
    setSubmitted(true);
    setCreationError(undefined);
    if (!result.definition) {
      if (result.errors.cores || result.errors.memory || result.errors.disk) {
        setResourceSettingsOpen(true);
      }
      return;
    }

    setCreating(true);
    try {
      await onCreateWorkspace({
        name: draft.name.trim() as workspaces.WorkspaceName,
        definition: {
          configToml: result.definition.configToml,
          dockerfile: result.definition.dockerfile,
          agentsMd: result.definition.agentsMd,
        },
        initialSecrets: result.definition.initialSecrets,
      });
    } catch (cause) {
      setCreationError(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setCreating(false);
    }
  };

  const update = <Key extends keyof WorkspaceCreationDraft>(
    key: Key,
    value: WorkspaceCreationDraft[Key],
  ) => {
    setDraft((current) => ({ ...current, [key]: value }));
  };

  const toggleStack = (stack: StackId) => {
    update(
      "stacks",
      draft.stacks.includes(stack)
        ? draft.stacks.filter((selected) => selected !== stack)
        : [...draft.stacks, stack],
    );
  };

  const toggleDeveloperTool = (tool: DeveloperToolId) => {
    update(
      "developerTools",
      draft.developerTools.includes(tool)
        ? draft.developerTools.filter((selected) => selected !== tool)
        : [...draft.developerTools, tool],
    );
  };

  const toggleFeature = (feature: WorkspaceFeatureId) => {
    update("features", {
      ...draft.features,
      [feature]: !draft.features[feature],
    });
  };

  const updateDeveloperService = (
    service: DeveloperServiceId,
    value: Partial<WorkspaceCreationDraft["developerServices"][DeveloperServiceId]>,
  ) => {
    setDraft((current) => ({
      ...current,
      developerServices: {
        ...current.developerServices,
        [service]: {
          ...current.developerServices[service],
          ...value,
        },
      },
    }));
  };

  const addRepository = () => {
    const id = `repository-${nextRepositoryId.current}`;
    nextRepositoryId.current += 1;
    update("repositories", [...draft.repositories, {
      id,
      source: "",
      path: "",
      branch: "",
    }]);
  };

  const updateRepository = (
    id: string,
    value: Partial<Omit<WorkspaceRepositoryDraft, "id">>,
  ) => {
    update(
      "repositories",
      draft.repositories.map((repository) =>
        repository.id === id ? { ...repository, ...value } : repository),
    );
  };

  const removeRepository = (id: string) => {
    update(
      "repositories",
      draft.repositories.filter((repository) => repository.id !== id),
    );
  };

  return (
    <main className="h-full min-h-0 overflow-y-auto bg-canvas text-foreground">
      <form className="mx-auto w-full max-w-[88rem] px-5 py-6 sm:px-8 lg:py-9" onSubmit={(event) => void submit(event)}>
        <header className="flex flex-col gap-5 border-b border-ui-border pb-6 sm:flex-row sm:items-start sm:justify-between">
          <div className="flex min-w-0 items-start gap-3.5">
            <TascarrelLogo className="mt-0.5 size-9 shrink-0" />
            <div>
              <h1 className="text-xl font-semibold tracking-tight">Create a workspace</h1>
              <p className="mt-1.5 max-w-2xl text-xs leading-5 text-muted">
                Add your repositories and choose the tools the project needs.
              </p>
            </div>
          </div>
          {canCancel ? (
            <Button className="self-start" disabled={creating} onClick={onCancel}>Cancel</Button>
          ) : null}
        </header>

        <div className="mt-7 grid min-w-0 gap-8 xl:grid-cols-[minmax(0,1fr)_26rem]">
          <div className="min-w-0 divide-y divide-ui-border">
            <CreationSection
              title="Workspace"
            >
              <Field
                className="max-w-md"
                label="Workspace name"
                error={submitted ? result.errors.name : undefined}
                htmlFor="workspace-name"
              >
                <TextInput
                  id="workspace-name"
                  autoFocus
                  className="w-full"
                  autoComplete="off"
                  value={draft.name}
                  placeholder="my-project"
                  aria-invalid={submitted && Boolean(result.errors.name)}
                  aria-describedby={submitted && result.errors.name ? "workspace-name-error" : undefined}
                  disabled={creating}
                  onChange={(event) => update("name", event.target.value)}
                />
              </Field>

              <details
                className="group mt-4 max-w-3xl rounded-lg border border-ui-border bg-surface/40"
                open={resourceSettingsOpen}
                onToggle={(event) => setResourceSettingsOpen(event.currentTarget.open)}
              >
                <summary className="flex cursor-pointer list-none items-center justify-between gap-3 px-3.5 py-3 text-xs font-medium text-muted outline-none transition hover:text-foreground focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-accent">
                  <span>Resource sizing</span>
                  <span className="flex items-center gap-2 text-[10px] font-normal text-subtle">
                    {draft.cores || draft.memory || draft.disk ? "Custom" : "Automatic"}
                    <ChevronDown
                      aria-hidden="true"
                      className="transition-transform group-open:rotate-180"
                      size={14}
                    />
                  </span>
                </summary>
                <div className="grid gap-4 border-t border-ui-border px-3.5 py-4 sm:grid-cols-3">
                  <Field
                    label="CPU cores"
                    error={submitted ? result.errors.cores : undefined}
                    htmlFor="workspace-cores"
                  >
                    <TextInput
                      id="workspace-cores"
                      className="w-full"
                      inputMode="numeric"
                      value={draft.cores}
                      placeholder="Automatic"
                      aria-invalid={submitted && Boolean(result.errors.cores)}
                      aria-describedby={submitted && result.errors.cores ? "workspace-cores-error" : undefined}
                      disabled={creating}
                      onChange={(event) => update("cores", event.target.value)}
                    />
                  </Field>
                  <Field
                    label="Memory"
                    error={submitted ? result.errors.memory : undefined}
                    htmlFor="workspace-memory"
                  >
                    <TextInput
                      id="workspace-memory"
                      className="w-full"
                      value={draft.memory}
                      placeholder="Automatic"
                      aria-invalid={submitted && Boolean(result.errors.memory)}
                      aria-describedby={submitted && result.errors.memory ? "workspace-memory-error" : undefined}
                      disabled={creating}
                      onChange={(event) => update("memory", event.target.value)}
                    />
                  </Field>
                  <Field
                    label="Disk"
                    error={submitted ? result.errors.disk : undefined}
                    htmlFor="workspace-disk"
                  >
                    <TextInput
                      id="workspace-disk"
                      className="w-full"
                      value={draft.disk}
                      placeholder="1 TiB sparse"
                      aria-invalid={submitted && Boolean(result.errors.disk)}
                      aria-describedby={submitted && result.errors.disk ? "workspace-disk-error" : undefined}
                      disabled={creating}
                      onChange={(event) => update("disk", event.target.value)}
                    />
                  </Field>
                </div>
              </details>
            </CreationSection>

            <CreationSection
              title="Repositories"
              description="Clone repositories into /workspace when the workspace is created."
            >
              {draft.repositories.length > 0 ? (
                <div className="grid gap-3">
                  {draft.repositories.map((repository, index) => {
                    const repositoryError = submitted
                      ? result.repositoryErrors[repository.id]
                      : undefined;
                    return (
                      <div
                        className="rounded-lg border border-ui-border bg-surface/50 p-3"
                        key={repository.id}
                      >
                        <div className="mb-3 flex items-center justify-between gap-3">
                          <h3 className="text-[10px] font-medium text-muted">
                            Repository {index + 1}
                          </h3>
                          <Button
                            aria-label={`Remove repository ${index + 1}`}
                            disabled={creating}
                            size="icon"
                            variant="danger"
                            onClick={() => removeRepository(repository.id)}
                          >
                            <Trash2 aria-hidden="true" size={13} />
                          </Button>
                        </div>
                        <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-[minmax(0,1.4fr)_minmax(0,0.8fr)_minmax(0,0.8fr)]">
                          <Field
                            className="sm:col-span-2 lg:col-span-1"
                            label="Repository URL"
                            error={repositoryError?.source}
                            htmlFor={`${repository.id}-source`}
                          >
                            <TextInput
                              id={`${repository.id}-source`}
                              className="w-full font-mono"
                              autoComplete="off"
                              spellCheck={false}
                              value={repository.source}
                              placeholder="git@github.com:organization/project.git"
                              aria-invalid={Boolean(repositoryError?.source)}
                              aria-describedby={repositoryError?.source
                                ? `${repository.id}-source-error`
                                : undefined}
                              disabled={creating}
                              onChange={(event) =>
                                updateRepository(repository.id, { source: event.target.value })}
                            />
                          </Field>
                          <Field
                            label="Folder"
                            hint="Inferred"
                            error={repositoryError?.path}
                            htmlFor={`${repository.id}-path`}
                          >
                            <TextInput
                              id={`${repository.id}-path`}
                              className="w-full font-mono"
                              autoComplete="off"
                              spellCheck={false}
                              value={repository.path}
                              placeholder="project"
                              aria-invalid={Boolean(repositoryError?.path)}
                              aria-describedby={repositoryError?.path
                                ? `${repository.id}-path-error`
                                : undefined}
                              disabled={creating}
                              onChange={(event) =>
                                updateRepository(repository.id, { path: event.target.value })}
                            />
                          </Field>
                          <Field
                            label="Branch"
                            hint="Default branch"
                            error={repositoryError?.branch}
                            htmlFor={`${repository.id}-branch`}
                          >
                            <TextInput
                              id={`${repository.id}-branch`}
                              className="w-full font-mono"
                              autoComplete="off"
                              spellCheck={false}
                              value={repository.branch}
                              placeholder="main"
                              aria-invalid={Boolean(repositoryError?.branch)}
                              aria-describedby={repositoryError?.branch
                                ? `${repository.id}-branch-error`
                                : undefined}
                              disabled={creating}
                              onChange={(event) =>
                                updateRepository(repository.id, { branch: event.target.value })}
                            />
                          </Field>
                        </div>
                      </div>
                    );
                  })}
                </div>
              ) : null}
              <div className={draft.repositories.length > 0 ? "mt-3" : ""}>
                <Button disabled={creating} size="small" onClick={addRepository}>
                  <Plus aria-hidden="true" size={13} />
                  Add repository
                </Button>
              </div>
            </CreationSection>

            <CreationSection
              title="Development environment"
            >
              <h3 className="mb-2.5 text-[11px] font-medium text-muted">Languages</h3>
              <div className="grid gap-2.5 sm:grid-cols-2 lg:grid-cols-3">
                {STACK_OPTIONS.map((option) => (
                  <OptionToggle
                    key={option.id}
                    checked={draft.stacks.includes(option.id)}
                    disabled={creating}
                    label={option.label}
                    description={option.description}
                    onChange={() => toggleStack(option.id)}
                  />
                ))}
              </div>

              <h3 className="mb-2.5 mt-5 text-[11px] font-medium text-muted">Tools</h3>
              <div className="grid gap-2.5 sm:grid-cols-2 lg:grid-cols-3">
                {DEVELOPER_TOOL_OPTIONS.map((option) => (
                  <OptionToggle
                    key={option.id}
                    checked={draft.developerTools.includes(option.id)}
                    disabled={creating}
                    label={option.label}
                    description={option.description}
                    onChange={() => toggleDeveloperTool(option.id)}
                  />
                ))}
              </div>

              <Field
                className="mt-5"
                label="Additional Debian packages"
                hint="Optional"
                error={submitted ? result.errors.additionalPackages : undefined}
                htmlFor="workspace-packages"
              >
                <TextInput
                  id="workspace-packages"
                  className="w-full"
                  value={draft.additionalPackages}
                  placeholder="sqlite3 shellcheck tree"
                  aria-invalid={submitted && Boolean(result.errors.additionalPackages)}
                  aria-describedby={submitted && result.errors.additionalPackages
                    ? "workspace-packages-error"
                    : undefined}
                  disabled={creating}
                  onChange={(event) => update("additionalPackages", event.target.value)}
                />
              </Field>
            </CreationSection>

            <CreationSection
              title="Git providers"
              description="Optionally install a provider CLI and give it read-only API access."
            >
              <div className="grid gap-2.5 sm:grid-cols-2">
                {DEVELOPER_SERVICE_OPTIONS.map((option) => (
                  <DeveloperServiceCard
                    key={option.id}
                    option={option}
                    enabled={draft.developerServices[option.id].enabled}
                    token={draft.developerServices[option.id].token}
                    error={submitted ? result.errors[option.tokenField] : undefined}
                    disabled={creating}
                    onEnabledChange={(enabled) =>
                      updateDeveloperService(option.id, { enabled })}
                    onTokenChange={(token) =>
                      updateDeveloperService(option.id, { token })}
                  />
                ))}
              </div>
              <p className="mt-3 text-[10px] leading-4 text-subtle">
                Tascarrel encrypts tokens with SOPS using your default SSH key and injects them only
                into matching API requests. Repository cloning uses your host Git credentials.
              </p>
            </CreationSection>

            <CreationSection
              title="Workspace capabilities"
            >
              <div className="grid gap-2.5 sm:grid-cols-2">
                {FEATURE_OPTIONS.map((option) => (
                  <OptionToggle
                    key={option.id}
                    checked={draft.features[option.id]}
                    disabled={creating}
                    label={option.label}
                    description={option.description}
                    onChange={() => toggleFeature(option.id)}
                  />
                ))}
              </div>
            </CreationSection>

            <CreationSection
              title="Network"
            >
              <fieldset>
                <legend className="sr-only">Outbound network policy</legend>
                <div className="grid gap-2.5 sm:grid-cols-2">
                  <NetworkOption
                    value="standard"
                    current={draft.networkMode}
                    disabled={creating}
                    label="Web access"
                    description="Allow HTTP and HTTPS while blocking local and private networks."
                    onChange={(value) => update("networkMode", value)}
                  />
                  <NetworkOption
                    value="restricted"
                    current={draft.networkMode}
                    disabled={creating}
                    label="Restricted"
                    description="Only allow hosts required by selected tools and those you add."
                    onChange={(value) => update("networkMode", value)}
                  />
                </div>
              </fieldset>

              {draft.networkMode === "restricted" ? (
                <Field
                  className="mt-5"
                  label="Allowed hostnames"
                  hint="Optional"
                  error={submitted ? result.errors.allowedHosts : undefined}
                  htmlFor="workspace-allowed-hosts"
                >
                  <TextInput
                    id="workspace-allowed-hosts"
                    className="w-full"
                    value={draft.allowedHosts}
                    placeholder="github.com *.githubusercontent.com"
                    aria-invalid={submitted && Boolean(result.errors.allowedHosts)}
                    aria-describedby={submitted && result.errors.allowedHosts
                      ? "workspace-allowed-hosts-error"
                      : undefined}
                    disabled={creating}
                    onChange={(event) => update("allowedHosts", event.target.value)}
                  />
                </Field>
              ) : null}

              <Field
                className="mt-5"
                label="Host port mappings"
                hint="Optional"
                error={submitted ? result.errors.hostPorts : undefined}
                htmlFor="workspace-host-ports"
              >
                <TextInput
                  id="workspace-host-ports"
                  className="w-full"
                  value={draft.hostPorts}
                  placeholder="3000, 5432:15432"
                  aria-invalid={submitted && Boolean(result.errors.hostPorts)}
                  aria-describedby={submitted && result.errors.hostPorts
                    ? "workspace-host-ports-error"
                    : undefined}
                  disabled={creating}
                  onChange={(event) => update("hostPorts", event.target.value)}
                />
                <p className="mt-1.5 text-[10px] leading-4 text-subtle">
                  Use a port directly or host:pod to map different ports.
                </p>
              </Field>
            </CreationSection>
          </div>

          <aside className="min-w-0 xl:sticky xl:top-8 xl:self-start" aria-label="Generated workspace files">
            <WorkspaceFilePreview definition={preview} />

            {creationError ? (
              <p className="mt-3 rounded-lg border border-red-500/20 bg-red-500/[0.06] px-3 py-2.5 text-xs leading-5 text-red-200" role="alert">
                {creationError}
              </p>
            ) : null}

            <div className="mt-4 flex items-center justify-end gap-3">
              <Button
                variant="primary"
                type="submit"
                disabled={creating}
              >
                {creating ? <LoaderCircle className="animate-spin" aria-hidden="true" size={13} /> : null}
                {creating ? "Creating…" : "Create workspace"}
              </Button>
            </div>
          </aside>
        </div>
      </form>
    </main>
  );
}

function WorkspaceFilePreview({
  definition,
}: {
  definition?: WorkspaceCreationDefinition;
}) {
  const invalidPreview = "Fix the highlighted fields to update this preview.";
  const files = [
    {
      value: "dockerfile",
      label: "Dockerfile",
      content: definition?.dockerfile ?? invalidPreview,
    },
    {
      value: "config",
      label: "config.toml",
      content: definition?.configToml || "# Tascarrel defaults\n",
    },
    {
      value: "agents",
      label: "AGENTS.md",
      content: definition?.agentsMd ?? invalidPreview,
    },
  ] as const;

  return (
    <Tabs.Root
      className="overflow-hidden rounded-xl border border-ui-border bg-surface"
      defaultValue="dockerfile"
    >
      <div className="flex items-center justify-between gap-3 border-b border-ui-border px-4 py-3.5">
        <h2 className="text-xs font-semibold">Generated files</h2>
        <Badge size="xs" tone="primary">Live preview</Badge>
      </div>
      <Tabs.List className="flex border-b border-ui-border" aria-label="Generated file preview">
        {files.map((file) => (
          <Tabs.Tab
            className="flex-1 border-b-2 border-transparent px-3 py-2 text-[10px] font-medium text-subtle outline-none transition hover:bg-surface-raised hover:text-muted focus-visible:outline-2 focus-visible:outline-offset-[-2px] focus-visible:outline-accent data-[active]:border-accent data-[active]:bg-accent/[0.04] data-[active]:text-foreground"
            key={file.value}
            value={file.value}
          >
            {file.label}
          </Tabs.Tab>
        ))}
      </Tabs.List>
      {files.map((file) => (
        <Tabs.Panel className="outline-none" key={file.value} value={file.value}>
          <pre
            className="m-0 max-h-[34rem] min-h-72 overflow-auto bg-canvas p-4 font-mono text-[10px] leading-5 text-muted"
            aria-label={`${file.label} preview`}
          >
            <code>{file.content}</code>
          </pre>
        </Tabs.Panel>
      ))}
    </Tabs.Root>
  );
}

function CreationSection({
  title,
  description,
  children,
}: {
  title: string;
  description?: string;
  children: ReactNode;
}) {
  return (
    <section className="py-7 first:pt-0" aria-labelledby={`creation-${title.toLowerCase().replaceAll(/[^a-z]+/g, "-")}`}>
      <div className="mb-4">
        <h2
          className="text-sm font-semibold tracking-tight"
          id={`creation-${title.toLowerCase().replaceAll(/[^a-z]+/g, "-")}`}
        >
          {title}
        </h2>
        {description ? (
          <p className="mt-1 max-w-3xl text-[11px] leading-4 text-subtle">{description}</p>
        ) : null}
      </div>
      {children}
    </section>
  );
}

function Field({
  label,
  hint,
  error,
  htmlFor,
  className = "",
  children,
}: {
  label: string;
  hint?: string;
  error?: string;
  htmlFor: string;
  className?: string;
  children: ReactNode;
}) {
  return (
    <div className={className}>
      <div className="mb-1.5 flex items-baseline justify-between gap-2">
        <label className="text-[10px] font-medium text-muted" htmlFor={htmlFor}>{label}</label>
        {hint ? <span className="text-[9px] text-subtle">{hint}</span> : null}
      </div>
      {children}
      {error ? (
        <p className="mt-1.5 text-[10px] leading-4 text-red-300" id={`${htmlFor}-error`} role="alert">
          {error}
        </p>
      ) : null}
    </div>
  );
}

function OptionToggle({
  checked,
  disabled,
  label,
  description,
  onChange,
}: {
  checked: boolean;
  disabled: boolean;
  label: string;
  description: string;
  onChange: () => void;
}) {
  return (
    <label
      className={`flex cursor-pointer items-start gap-3 rounded-lg border px-3 py-3 outline-none transition ${
        checked
          ? "border-accent/45 bg-accent/[0.07]"
          : "border-ui-border bg-surface/50 hover:border-ui-border-strong hover:bg-surface"
      } has-[:focus-visible]:outline-2 has-[:focus-visible]:outline-offset-2 has-[:focus-visible]:outline-accent has-[:disabled]:cursor-not-allowed has-[:disabled]:opacity-50`}
    >
      <input
        className="mt-0.5 size-3.5 shrink-0 accent-accent"
        type="checkbox"
        checked={checked}
        disabled={disabled}
        onChange={onChange}
      />
      <span className="min-w-0">
        <span className="block text-xs font-medium text-foreground">{label}</span>
        <span className="mt-0.5 block text-[11px] leading-4 text-muted">{description}</span>
      </span>
    </label>
  );
}

function DeveloperServiceCard({
  option,
  enabled,
  token,
  error,
  disabled,
  onEnabledChange,
  onTokenChange,
}: {
  option: DeveloperServiceOption;
  enabled: boolean;
  token: string;
  error?: string;
  disabled: boolean;
  onEnabledChange: (enabled: boolean) => void;
  onTokenChange: (token: string) => void;
}) {
  const tokenInputId = `workspace-${option.id}-token`;
  return (
    <div
      className={`overflow-hidden rounded-lg border transition ${
        enabled
          ? "border-accent/45 bg-accent/[0.07]"
          : "border-ui-border bg-surface/50 hover:border-ui-border-strong hover:bg-surface"
      }`}
    >
      <label className="flex cursor-pointer items-start gap-3 px-3 py-3 has-[:disabled]:cursor-not-allowed has-[:disabled]:opacity-50">
        <input
          className="mt-0.5 size-3.5 shrink-0 accent-accent"
          type="checkbox"
          checked={enabled}
          disabled={disabled}
          aria-controls={`${tokenInputId}-settings`}
          onChange={(event) => onEnabledChange(event.target.checked)}
        />
        <span className="min-w-0">
          <span className="block text-xs font-medium text-foreground">{option.label}</span>
          <span className="mt-0.5 block text-[11px] leading-4 text-muted">
            {option.description}
          </span>
        </span>
      </label>
      {enabled ? (
        <div className="border-t border-accent/20 px-3 py-3" id={`${tokenInputId}-settings`}>
          <Field
            label="Read-only token"
            hint="Required"
            error={error}
            htmlFor={tokenInputId}
          >
            <TextInput
              id={tokenInputId}
              className="w-full font-mono"
              type="password"
              autoComplete="new-password"
              spellCheck={false}
              value={token}
              placeholder={option.id === "github" ? "github_pat_…" : "glpat-…"}
              aria-invalid={Boolean(error)}
              aria-describedby={error ? `${tokenInputId}-error` : undefined}
              disabled={disabled}
              onChange={(event) => onTokenChange(event.target.value)}
            />
          </Field>
        </div>
      ) : null}
    </div>
  );
}

function NetworkOption({
  value,
  current,
  disabled,
  label,
  description,
  onChange,
}: {
  value: NetworkMode;
  current: NetworkMode;
  disabled: boolean;
  label: string;
  description: string;
  onChange: (value: NetworkMode) => void;
}) {
  const checked = value === current;
  return (
    <label
      className={`flex cursor-pointer items-start gap-3 rounded-lg border px-3 py-3 transition ${
        checked
          ? "border-accent/45 bg-accent/[0.07]"
          : "border-ui-border bg-surface/50 hover:border-ui-border-strong hover:bg-surface"
      } has-[:focus-visible]:outline-2 has-[:focus-visible]:outline-offset-2 has-[:focus-visible]:outline-accent has-[:disabled]:cursor-not-allowed has-[:disabled]:opacity-50`}
    >
      <input
        className="mt-0.5 size-3.5 shrink-0 accent-accent"
        type="radio"
        name="network-mode"
        value={value}
        checked={checked}
        disabled={disabled}
        onChange={() => onChange(value)}
      />
      <span>
        <span className="block text-xs font-medium text-foreground">{label}</span>
        <span className="mt-0.5 block text-[11px] leading-4 text-muted">{description}</span>
      </span>
    </label>
  );
}
