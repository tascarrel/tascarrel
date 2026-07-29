import { Box } from "lucide-react";
import type { ReactNode } from "react";

import type {
  changes,
  pods,
  repositories,
  workspaces,
} from "../../../api/generated/index.ts";
import { Badge, type BadgeTone } from "../../../components/ui/Badge.tsx";
import { SidebarTabs, SidebarTabsPanel } from "../../../components/ui/SidebarTabs.tsx";
import { PodRepositories } from "../../pods/PodRepositories.tsx";
import { ShellPlaceholder } from "./ShellPlaceholder.tsx";

export function PodOverview({
  workspace,
  pod,
  processView,
  repositories: configuredRepositories = [],
  repositoriesReady = false,
  repositoriesError,
  repositoryStatuses = [],
  repositoryStatusesReady = false,
}: {
  workspace?: workspaces.WorkspaceName;
  pod?: pods.Pod;
  processView?: ReactNode;
  repositories?: readonly repositories.Repository[];
  repositoriesReady?: boolean;
  repositoriesError?: string;
  repositoryStatuses?: readonly changes.RepositoryStatusEntry[];
  repositoryStatusesReady?: boolean;
}) {
  if (!pod) {
    return (
      <ShellPlaceholder
        icon={Box}
        title="No pod selected"
        detail="Select a running pod from the workspace sidebar."
      />
    );
  }
  const failure = pod.status.status === "Failed" ? pod.status : undefined;
  return (
    <div className="flex h-full min-h-0 flex-col overflow-hidden bg-canvas text-foreground">
      <SidebarTabs
        ariaLabel="Pod sections"
        defaultValue="overview"
        items={[
          {
            value: "overview",
            label: "Overview",
          },
          ...(processView
            ? [{
                value: "processes",
                label: "Processes",
              }]
            : []),
          {
            value: "repositories",
            label: "Repositories",
          },
        ]}
      >
        <SidebarTabsPanel contentClassName="max-w-5xl" value="overview">
          <section aria-labelledby="pod-overview-title">
            <header className="mb-6 flex flex-wrap items-start justify-between gap-4">
              <div>
                <h2 className="text-sm font-semibold text-foreground" id="pod-overview-title">
                  {pod.title || "Untitled pod"}
                </h2>
                <p className="mt-1 text-xs leading-5 text-subtle">
                  Pod details and current lifecycle state.
                </p>
              </div>
              <Badge size="xs" tone={podStatusTone(pod.status.status)}>
                {pod.status.status}
              </Badge>
            </header>

            {failure ? (
              <div className="mb-6 border-l-2 border-red-400 bg-red-500/5 px-4 py-3 text-red-200" role="alert">
                <h3 className="text-xs font-semibold">Pod operation failed</h3>
                <pre className="mt-2 whitespace-pre-wrap break-words font-mono text-[10px] leading-5">{failure.message}</pre>
                <p className="mt-2 text-[10px] text-red-300">
                  {new Date(String(failure.failedAt)).toLocaleString()}
                </p>
              </div>
            ) : null}

            <dl className="grid gap-x-10 gap-y-5 text-xs sm:grid-cols-2">
              <PodDetail label="Workspace" value={workspace ?? "—"} />
              <PodDetail label="Created" value={new Date(String(pod.createdAt)).toLocaleString()} />
              <PodDetail className="sm:col-span-2" label="Pod ID" value={pod.id} />
            </dl>
          </section>
        </SidebarTabsPanel>

        {processView ? (
          <SidebarTabsPanel contentClassName="max-w-6xl" value="processes">
            {processView}
          </SidebarTabsPanel>
        ) : null}

        {workspace ? (
          <SidebarTabsPanel contentClassName="max-w-5xl" value="repositories">
            <PodRepositories
              workspace={workspace}
              pod={pod}
              repositories={configuredRepositories}
              repositoriesReady={repositoriesReady}
              repositoriesError={repositoriesError}
              statuses={repositoryStatuses}
              statusesReady={repositoryStatusesReady}
            />
          </SidebarTabsPanel>
        ) : null}
      </SidebarTabs>
    </div>
  );
}

function PodDetail({
  className,
  label,
  value,
}: {
  className?: string;
  label: string;
  value: string;
}) {
  return (
    <div className={`min-w-0 ${className ?? ""}`}>
      <dt className="text-subtle">{label}</dt>
      <dd className="mt-1.5 min-w-0 break-all font-mono text-[11px] text-muted">{value}</dd>
    </div>
  );
}

function podStatusTone(status: pods.PodState["status"]): BadgeTone {
  if (status === "Running") return "success";
  if (status === "Failed") return "danger";
  if (status === "Stopping" || status === "Destroying") return "warning";
  if (status === "Stopped") return "muted";
  return "primary";
}
