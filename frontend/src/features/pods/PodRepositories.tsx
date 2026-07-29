import { LoaderCircle } from "lucide-react";
import { useEffect, useMemo, useState } from "react";

import { guestApi } from "../../api/client.ts";
import type {
  changes,
  pods,
  repositories,
  workspaces,
} from "../../api/generated/index.ts";
import { Badge } from "../../components/ui/Badge.tsx";
import { Button } from "../../components/ui/Button.tsx";

export function PodRepositories({
  workspace,
  pod,
  repositories: configuredRepositories,
  repositoriesReady,
  repositoriesError,
  statuses,
  statusesReady,
}: {
  workspace: workspaces.WorkspaceName;
  pod: pods.Pod;
  repositories: readonly repositories.Repository[];
  repositoriesReady: boolean;
  repositoriesError?: string;
  statuses: readonly changes.RepositoryStatusEntry[];
  statusesReady: boolean;
}) {
  const [importingPath, setImportingPath] = useState<string>();
  const [availablePaths, setAvailablePaths] = useState<ReadonlySet<string>>(
    () => new Set(),
  );
  const [actionError, setActionError] = useState<string>();
  const statusesByPath = useMemo(
    () => new Map(
      statuses
        .filter((entry) => entry.target.podId === pod.id)
        .map((entry) => [String(entry.target.path), entry]),
    ),
    [pod.id, statuses],
  );

  useEffect(() => {
    setImportingPath(undefined);
    setAvailablePaths(new Set());
    setActionError(undefined);
  }, [pod.id, workspace]);

  useEffect(() => {
    setAvailablePaths((current) => {
      const pending = [...current].filter(
        (path) => statusesByPath.get(path)?.state.status !== "Ready",
      );
      return pending.length === current.size ? current : new Set(pending);
    });
  }, [statusesByPath]);

  const importRepository = async (path: string) => {
    if (importingPath) return;
    setImportingPath(path);
    setActionError(undefined);
    try {
      const output = await guestApi(workspace).execute("pods_ImportRepository", {
        podId: pod.id,
        path,
      });
      if (output.result.status === "DestinationOccupied") {
        setActionError(
          `/workspace/${path} is occupied. Existing files were left unchanged.`,
        );
        return;
      }
      setAvailablePaths((current) => new Set(current).add(path));
    } catch (cause) {
      setActionError(errorMessage(cause));
    } finally {
      setImportingPath(undefined);
    }
  };

  return (
    <section aria-labelledby="pod-repositories-title">
      <header className="mb-6">
        <h2 className="text-sm font-semibold text-foreground" id="pod-repositories-title">
          Repositories
        </h2>
        <p className="mt-1 text-xs leading-5 text-subtle">
          Import repositories added to the workspace after this pod was created.
          Existing destinations are never replaced.
        </p>
      </header>

      {pod.status.status !== "Running" ? (
        <p className="mb-4 border-l-2 border-amber-400 bg-amber-500/5 px-4 py-3 text-xs leading-5 text-amber-200">
          Start this pod before importing a repository.
        </p>
      ) : null}

      {actionError || repositoriesError ? (
        <p
          className="mb-4 border-l-2 border-red-400 bg-red-500/5 px-4 py-3 text-xs leading-5 text-red-200"
          role="alert"
        >
          {actionError ?? repositoriesError}
        </p>
      ) : null}

      {configuredRepositories.length ? (
        <ul className="grid list-none gap-3 p-0">
          {configuredRepositories.map((repository) => {
            const status = statusesByPath.get(repository.path);
            const available = availablePaths.has(repository.path)
              || status?.state.status === "Ready";
            const failed = status?.state.status === "Failed" ? status.state : undefined;
            const importing = importingPath === repository.path;
            const checking = !status && !statusesReady;
            return (
              <li
                className="flex flex-wrap items-start justify-between gap-4 rounded-xl border border-ui-border bg-surface/60 px-4 py-3"
                key={repository.path}
              >
                <div className="min-w-0 flex-1">
                  <div className="flex flex-wrap items-center gap-2">
                    <h3 className="font-mono text-xs font-semibold text-foreground">
                      /workspace/{repository.path}
                    </h3>
                    <Badge
                      size="xs"
                      tone={available
                        ? "success"
                        : failed
                          ? "danger"
                          : checking
                            ? "muted"
                            : "warning"}
                    >
                      {available
                        ? "Present"
                        : failed
                          ? "Conflict"
                          : checking
                            ? "Checking"
                            : "Not in pod"}
                    </Badge>
                  </div>
                  <p className="mt-1 break-all font-mono text-[10px] leading-4 text-subtle">
                    {repository.source}
                  </p>
                  {status?.state.status === "Ready" ? (
                    <p className="mt-2 text-[11px] text-muted">
                      {status.state.branch
                        ? `Checked out on ${status.state.branch}.`
                        : "The checkout has a detached or unborn HEAD."}
                    </p>
                  ) : failed ? (
                    <p className="mt-2 text-[11px] leading-5 text-red-200">
                      {failed.message}
                    </p>
                  ) : !available && !checking ? (
                    <p className="mt-2 text-[11px] text-muted">
                      The configured checkout is absent from this pod.
                    </p>
                  ) : null}
                </div>

                {!available && !failed ? (
                  <Button
                    size="small"
                    variant="primary"
                    disabled={pod.status.status !== "Running" || checking || Boolean(importingPath)}
                    onClick={() => void importRepository(repository.path)}
                  >
                    {importing ? (
                      <LoaderCircle aria-hidden="true" className="size-3.5 animate-spin" />
                    ) : null}
                    {importing ? "Importing…" : "Import"}
                  </Button>
                ) : null}
              </li>
            );
          })}
        </ul>
      ) : (
        <div className="rounded-xl border border-dashed border-ui-border p-8 text-center text-xs leading-5 text-subtle">
          {repositoriesReady
            ? "No repositories are configured for this workspace."
            : repositoriesError
              ? "The configured repository inventory is unavailable."
              : "Loading configured repositories…"}
        </div>
      )}
    </section>
  );
}

function errorMessage(cause: unknown): string {
  if (cause instanceof Error) return cause.message;
  if (typeof cause === "string") return cause;
  return "Repository import failed.";
}
