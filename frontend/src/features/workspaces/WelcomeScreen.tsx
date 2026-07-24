import { LoaderCircle } from "lucide-react";
import { useState, type FormEvent } from "react";

import type { workspaces } from "../../api/generated/index.ts";
import { Button } from "../../components/ui/Button.tsx";
import { TextInput } from "../../components/ui/TextInput.tsx";
import { TascarrelLogo } from "../../components/ui/TascarrelLogo.tsx";

export function WelcomeScreen({
  onCreateWorkspace,
}: {
  onCreateWorkspace: (workspace: workspaces.WorkspaceName) => Promise<void>;
}) {
  const [name, setName] = useState("");
  const [creating, setCreating] = useState(false);
  const [error, setError] = useState<string>();

  const createWorkspace = async (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    const workspaceName = name.trim();
    if (!workspaceName || creating) return;

    setCreating(true);
    setError(undefined);
    try {
      await onCreateWorkspace(workspaceName as workspaces.WorkspaceName);
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setCreating(false);
    }
  };

  return (
    <main className="flex h-full min-h-0 items-center justify-center bg-canvas px-8 text-foreground">
      <section className="w-full max-w-sm text-center">
        <TascarrelLogo className="mx-auto size-14" />
        <h1 className="mt-5 text-lg font-semibold tracking-tight">Welcome to Tascarrel</h1>
        <p className="mt-2 text-xs text-muted">Create your first workspace to get started.</p>

        <form className="mx-auto mt-6 flex max-w-xs gap-2" onSubmit={(event) => void createWorkspace(event)}>
          <label className="sr-only" htmlFor="welcome-workspace-name">Workspace name</label>
          <TextInput
            className="flex-1"
            id="welcome-workspace-name"
            autoFocus
            value={name}
            placeholder="Workspace name"
            pattern="[A-Za-z0-9_\-]{1,64}"
            disabled={creating}
            onChange={(event) => setName(event.target.value)}
          />
          <Button variant="primary" type="submit" disabled={creating || !name.trim()}>
            {creating ? <LoaderCircle className="animate-spin" aria-hidden="true" size={13} /> : null}
            Create
          </Button>
        </form>
        {error ? <p className="mt-3 text-xs text-red-300" role="alert">{error}</p> : null}
      </section>
    </main>
  );
}
