import { Plus } from "lucide-react";
import type { ReactNode } from "react";

import type { pods } from "../../../api/generated/index.ts";
import { Button } from "../../../components/ui/Button.tsx";
import type { PodChangeSummary } from "../../changes/podChangeSummary.ts";
import {
  MobilePodRow,
  MobileSectionHeading,
  type MobileChatSummary,
} from "./MobilePodList.tsx";

export function MobileWorkspaceScreen({
  pods: workspacePods,
  chats: workspaceChats,
  podChangeSummaries,
  approvalsView,
  onCreatePod,
  onSelectPod,
}: {
  pods: readonly pods.Pod[];
  chats: readonly MobileChatSummary[];
  podChangeSummaries: ReadonlyMap<pods.PodId, PodChangeSummary>;
  approvalsView: ReactNode;
  onCreatePod: () => void;
  onSelectPod: (podId: pods.PodId) => void;
}) {
  return (
    <div className="mobile-client-content min-h-0 flex-1 overflow-y-auto pt-4">
      <div className="mx-auto grid w-full min-w-0 max-w-2xl gap-6">
        <Button className="h-12 w-full rounded-xl text-sm" variant="primary" onClick={onCreatePod}>
          <Plus aria-hidden="true" className="size-4" />
          Start a New Pod
        </Button>

        {approvalsView}

        <section className="min-w-0" aria-labelledby="mobile-pods-title">
          <MobileSectionHeading
            id="mobile-pods-title"
            title="Pods"
            count={workspacePods.length}
          />
          <div className="mt-3 grid min-w-0 gap-2">
            {workspacePods
              .toSorted((left, right) =>
                String(right.createdAt).localeCompare(String(left.createdAt))
              )
              .map((pod) => (
                <MobilePodRow
                  key={pod.id}
                  pod={pod}
                  changeSummary={podChangeSummaries.get(pod.id)}
                  attention={workspaceChats.some((chat) =>
                    chat.podId === pod.id
                    && (chat.attention || chat.status === "needs-input" || chat.status === "failed")
                  )}
                  working={workspaceChats.some(
                    (chat) => chat.podId === pod.id && chat.status === "working",
                  )}
                  onClick={() => onSelectPod(pod.id)}
                />
              ))}
            {!workspacePods.length ? (
              <div className="rounded-2xl border border-dashed border-ui-border p-6 text-center text-sm leading-6 text-subtle">
                No pods yet. Start a new pod above.
              </div>
            ) : null}
          </div>
        </section>
      </div>
    </div>
  );
}
