import { Eye, EyeOff, Pencil, Plus, Save, Trash2, X } from "lucide-react";
import { useEffect, useRef, useState, type FormEvent } from "react";

import { hostApi } from "../../api/client.ts";
import type { secrets, workspaces } from "../../api/generated/index.ts";
import { Badge } from "../../components/ui/Badge.tsx";
import { Button } from "../../components/ui/Button.tsx";
import { ConfirmDialog } from "../../components/ui/ConfirmDialog.tsx";
import { TextInput } from "../../components/ui/TextInput.tsx";
import { useWorkspaceSecrets } from "./state.ts";

const REVEAL_DURATION_MS = 30_000;

type DeleteTarget = {
  provider: secrets.SecretProviderMetadata;
  secretName: string;
};

export function SecretsSettings({ workspace }: { workspace: workspaces.WorkspaceName }) {
  const state = useWorkspaceSecrets(workspace);
  const [actionError, setActionError] = useState<string>();
  const [revealed, setRevealed] = useState<Record<string, string>>({});
  const [revisions, setRevisions] = useState<Record<string, string>>({});
  const [deleteTarget, setDeleteTarget] = useState<DeleteTarget>();
  const [deleting, setDeleting] = useState(false);
  const revealTimers = useRef(new Map<string, ReturnType<typeof setTimeout>>());

  useEffect(() => () => {
    for (const timer of revealTimers.current.values()) clearTimeout(timer);
    revealTimers.current.clear();
  }, []);

  useEffect(() => {
    for (const timer of revealTimers.current.values()) clearTimeout(timer);
    revealTimers.current.clear();
    setRevealed({});
    setRevisions({});
  }, [state.value]);

  const conceal = (providerName: string, secretName: string) => {
    const key = secretKey(providerName, secretName);
    const timer = revealTimers.current.get(key);
    if (timer) clearTimeout(timer);
    revealTimers.current.delete(key);
    setRevealed((current) => {
      if (!(key in current)) return current;
      const next = { ...current };
      delete next[key];
      return next;
    });
  };

  const reveal = async (providerName: string, secretName: string): Promise<string | undefined> => {
    const key = secretKey(providerName, secretName);
    const cached = revealed[key];
    if (cached !== undefined) return cached;
    setActionError(undefined);
    try {
      const result = await hostApi.execute("secrets_Reveal", {
        workspaceName: workspace,
        providerName,
        secretName,
      });
      setRevealed((current) => ({ ...current, [key]: result.value }));
      const previous = revealTimers.current.get(key);
      if (previous) clearTimeout(previous);
      revealTimers.current.set(key, setTimeout(() => conceal(providerName, secretName), REVEAL_DURATION_MS));
      return result.value;
    } catch (cause) {
      setActionError(errorMessage(cause));
      return undefined;
    }
  };

  const setSecret = async (
    provider: secrets.SecretProviderMetadata,
    secretName: string,
    value: string,
  ) => {
    setActionError(undefined);
    try {
      const expectedRevision = revisions[provider.name] ?? provider.revision;
      const result = await hostApi.execute("secrets_Set", {
        workspaceName: workspace,
        providerName: provider.name,
        secretName,
        value,
        ...(expectedRevision ? { expectedRevision } : {}),
      });
      setRevisions((current) => ({ ...current, [provider.name]: result.revision }));
      conceal(provider.name, secretName);
    } catch (cause) {
      setActionError(errorMessage(cause));
      throw cause;
    }
  };

  const deleteSecret = async () => {
    if (!deleteTarget || deleting) return;
    setDeleting(true);
    setActionError(undefined);
    try {
      const expectedRevision = revisions[deleteTarget.provider.name] ?? deleteTarget.provider.revision;
      const result = await hostApi.execute("secrets_Delete", {
        workspaceName: workspace,
        providerName: deleteTarget.provider.name,
        secretName: deleteTarget.secretName,
        ...(expectedRevision ? { expectedRevision } : {}),
      });
      setRevisions((current) => ({
        ...current,
        [deleteTarget.provider.name]: result.revision,
      }));
      conceal(deleteTarget.provider.name, deleteTarget.secretName);
      setDeleteTarget(undefined);
    } catch (cause) {
      setActionError(errorMessage(cause));
      setDeleteTarget(undefined);
    } finally {
      setDeleting(false);
    }
  };

  const providers = state.value?.providers ?? [];
  return (
    <div className="mx-auto max-w-4xl">
      <h2 className="text-sm font-semibold text-foreground">Secrets</h2>
      <p className="mt-1 max-w-2xl text-xs leading-5 text-subtle">
        Manage encrypted values through providers declared in <code>config.toml</code>. Revealed values are concealed after 30 seconds.
      </p>
      <p className="mt-2 max-w-2xl text-[11px] leading-5 text-subtle">
        Environment references use <code>{"${secrets.<provider>.<name>}"}</code> and take effect after restarting the workspace VM.
      </p>

      {actionError || state.error ? (
        <p className="mt-4 rounded-xl border border-red-500/20 bg-red-500/5 px-3 py-2 text-xs text-red-200" role="alert">
          {actionError ?? state.error?.message}
        </p>
      ) : null}

      {!state.ready && !state.value ? <p className="mt-5 text-xs text-subtle">Loading secret providers…</p> : null}
      {state.ready && providers.length === 0 ? (
        <div className="mt-5 rounded-xl border border-ui-border bg-surface p-4">
          <h3 className="text-xs font-semibold text-foreground">No Secret Providers Configured</h3>
          <p className="mt-1 text-[11px] leading-5 text-subtle">
            Add a named provider such as <code>[secrets.providers.project]</code> with <code>kind = "sops"</code> to the workspace configuration.
          </p>
        </div>
      ) : null}

      <div className="mt-5 grid gap-4">
        {providers.map((provider) => (
          <ProviderCard
            key={provider.name}
            provider={provider}
            revealed={revealed}
            onConceal={conceal}
            onDelete={(secretName) => setDeleteTarget({ provider, secretName })}
            onReveal={reveal}
            onSet={setSecret}
          />
        ))}
      </div>

      <ConfirmDialog
        confirmLabel="Delete secret"
        description={deleteTarget
          ? `Delete ${deleteTarget.provider.name}.${deleteTarget.secretName} from its encrypted provider?`
          : "Delete this secret?"}
        destructive
        open={deleteTarget !== undefined}
        pending={deleting}
        title="Delete Secret?"
        onOpenChange={(open) => {
          if (!open) setDeleteTarget(undefined);
        }}
        onConfirm={() => void deleteSecret()}
      />
    </div>
  );
}

function ProviderCard({
  provider,
  revealed,
  onReveal,
  onConceal,
  onSet,
  onDelete,
}: {
  provider: secrets.SecretProviderMetadata;
  revealed: Record<string, string>;
  onReveal: (providerName: string, secretName: string) => Promise<string | undefined>;
  onConceal: (providerName: string, secretName: string) => void;
  onSet: (provider: secrets.SecretProviderMetadata, secretName: string, value: string) => Promise<void>;
  onDelete: (secretName: string) => void;
}) {
  const [secretName, setSecretName] = useState("");
  const [value, setValue] = useState("");
  const [editing, setEditing] = useState<string>();
  const [showValue, setShowValue] = useState(false);
  const [pending, setPending] = useState(false);

  useEffect(() => {
    if (!editing || revealed[secretKey(provider.name, editing)] !== undefined) return;
    setSecretName("");
    setValue("");
    setEditing(undefined);
    setShowValue(false);
  }, [editing, provider.name, revealed]);

  const resetEditor = () => {
    if (editing) onConceal(provider.name, editing);
    setSecretName("");
    setValue("");
    setEditing(undefined);
    setShowValue(false);
  };

  const edit = async (name: string) => {
    setPending(true);
    try {
      const current = await onReveal(provider.name, name);
      if (current === undefined) return;
      setSecretName(name);
      setValue(current);
      setEditing(name);
      setShowValue(false);
    } finally {
      setPending(false);
    }
  };

  const submit = async (event: FormEvent) => {
    event.preventDefault();
    if (!secretName || pending) return;
    setPending(true);
    try {
      await onSet(provider, secretName, value);
      resetEditor();
    } catch {
      return;
    } finally {
      setPending(false);
    }
  };

  return (
    <section className="rounded-xl border border-ui-border bg-surface p-4" aria-labelledby={`provider-${provider.name}`}>
      <div className="flex flex-wrap items-center justify-between gap-2">
        <div className="flex items-center gap-2">
          <h3 className="text-xs font-semibold text-foreground" id={`provider-${provider.name}`}>{provider.name}</h3>
          <Badge size="xs">{provider.kind}</Badge>
        </div>
        <span className="text-[10px] text-subtle">{provider.secrets.length} {provider.secrets.length === 1 ? "secret" : "secrets"}</span>
      </div>

      {provider.error ? (
        <p className="mt-3 rounded-lg border border-amber-500/20 bg-amber-500/5 px-3 py-2 text-[11px] text-amber-200" role="alert">
          {provider.error.message}
        </p>
      ) : null}

      <div className="mt-3 divide-y divide-ui-border/70 border-y border-ui-border/70">
        {provider.secrets.map((secret) => {
          const key = secretKey(provider.name, secret.name);
          const plaintext = revealed[key];
          return (
            <div className="flex min-h-11 items-center gap-3 py-2" key={secret.name}>
              <code className="min-w-0 flex-1 truncate text-[11px] text-foreground">{provider.name}.{secret.name}</code>
              <code className="max-w-[40%] truncate text-[11px] text-subtle" aria-label={plaintext === undefined ? "Secret concealed" : "Secret revealed"}>
                {plaintext === undefined ? "••••••••" : plaintext}
              </code>
              {provider.capabilities.reveal ? (
                <Button
                  aria-label={plaintext === undefined ? `Reveal ${secret.name}` : `Conceal ${secret.name}`}
                  size="icon"
                  disabled={pending || Boolean(provider.error)}
                  onClick={() => plaintext === undefined
                    ? void onReveal(provider.name, secret.name)
                    : onConceal(provider.name, secret.name)}
                >
                  {plaintext === undefined ? <Eye aria-hidden="true" className="size-3.5" /> : <EyeOff aria-hidden="true" className="size-3.5" />}
                </Button>
              ) : null}
              {provider.capabilities.set ? (
                <Button aria-label={`Edit ${secret.name}`} size="icon" disabled={pending || Boolean(provider.error)} onClick={() => void edit(secret.name)}>
                  <Pencil aria-hidden="true" className="size-3.5" />
                </Button>
              ) : null}
              {provider.capabilities.delete ? (
                <Button aria-label={`Delete ${secret.name}`} size="icon" variant="danger" disabled={pending || Boolean(provider.error)} onClick={() => onDelete(secret.name)}>
                  <Trash2 aria-hidden="true" className="size-3.5" />
                </Button>
              ) : null}
            </div>
          );
        })}
        {provider.secrets.length === 0 ? <p className="py-3 text-[11px] text-subtle">This provider contains no secrets.</p> : null}
      </div>

      {provider.capabilities.set ? (
        <form className="mt-4 grid gap-2 sm:grid-cols-[minmax(0,0.8fr)_minmax(0,1.2fr)_auto]" onSubmit={(event) => void submit(event)}>
          <TextInput
            aria-label="Secret name"
            autoComplete="off"
            disabled={pending || Boolean(provider.error) || editing !== undefined}
            pattern="[A-Za-z_][A-Za-z0-9_]*"
            placeholder="SECRET_NAME"
            required
            value={secretName}
            onChange={(event) => setSecretName(event.target.value)}
          />
          <div className="flex min-w-0">
            <TextInput
              aria-label="Secret value"
              autoComplete="new-password"
              className="w-full rounded-r-none"
              disabled={pending || Boolean(provider.error)}
              placeholder={editing ? "Replacement value" : "Secret value"}
              type={showValue ? "text" : "password"}
              value={value}
              onChange={(event) => setValue(event.target.value)}
            />
            <Button
              aria-label={showValue ? "Conceal value being edited" : "Reveal value being edited"}
              className="shrink-0 rounded-l-none border-l-0 focus-visible:outline-offset-0"
              disabled={pending || Boolean(provider.error)}
              size="icon"
              onClick={() => setShowValue((current) => !current)}
            >
              {showValue ? <EyeOff aria-hidden="true" className="size-3.5" /> : <Eye aria-hidden="true" className="size-3.5" />}
            </Button>
          </div>
          <div className="flex gap-2">
            <Button variant="primary" disabled={pending || Boolean(provider.error)} type="submit">
              {editing ? <Save aria-hidden="true" className="size-3.5" /> : <Plus aria-hidden="true" className="size-3.5" />}
              {editing ? "Save" : "Add"}
            </Button>
            {editing ? (
              <Button aria-label="Cancel editing" size="icon" disabled={pending} onClick={resetEditor}>
                <X aria-hidden="true" className="size-3.5" />
              </Button>
            ) : null}
          </div>
        </form>
      ) : null}
    </section>
  );
}

function secretKey(providerName: string, secretName: string) {
  return `${providerName}\0${secretName}`;
}

function errorMessage(cause: unknown) {
  return cause instanceof Error ? cause.message : String(cause);
}
