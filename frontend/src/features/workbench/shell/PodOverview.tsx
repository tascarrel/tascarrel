import { Box } from "lucide-react";
import type { ReactNode } from "react";

import type { pods, workspaces } from "../../../api/generated/index.ts";
import { ShellPlaceholder } from "./ShellPlaceholder.tsx";

export function PodOverview({
  workspace,
  pod,
  processView,
}: {
  workspace?: workspaces.WorkspaceName;
  pod?: pods.Pod;
  processView?: ReactNode;
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
    <div className="pod-overview">
      <div className="pod-overview-heading">
        <div><span>Pod</span><h1>{pod.title || "Untitled pod"}</h1><p>{workspace}</p></div>
        <span className="pod-overview-health" data-failed={failure ? true : undefined}>
          {pod.status.status}
        </span>
      </div>
      {failure ? (
        <section className="pod-overview-failure" role="alert">
          <h2>Pod operation failed</h2>
          <pre>{failure.message}</pre>
          <p>{new Date(String(failure.failedAt)).toLocaleString()}</p>
        </section>
      ) : null}
      <div className="pod-overview-metadata">
        <div><span>Workspace</span><strong>{workspace}</strong></div>
        <div><span>Pod ID</span><strong>{pod.id}</strong></div>
        <div><span>Status</span><strong>{pod.status.status}</strong></div>
        <div><span>Created</span><strong>{new Date(String(pod.createdAt)).toLocaleString()}</strong></div>
      </div>
      <section className="pod-overview-section">
        <h2>Workspace Pod</h2>
        <p>Agent chats associated with this pod are available in the Agent view.</p>
      </section>
      {processView ? <div className="mx-auto mt-7 max-w-[900px]">{processView}</div> : null}
    </div>
  );
}
