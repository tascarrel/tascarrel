import { Plus } from "lucide-react";
import { useMemo, type ReactNode } from "react";

import type { chats, pods, workspaces } from "../../../api/generated/index.ts";
import { Button } from "../../../components/ui/Button.tsx";
import type { PodChangeSummary } from "../../changes/podChangeSummary.ts";
import {
  MobileChatSection,
  MobilePodRow,
  MobileSectionHeading,
  type MobileChatSummary,
} from "./MobileTaskList.tsx";

export function MobileWorkspaceScreen({
  workspace,
  pods: workspacePods,
  chats: workspaceChats,
  podChangeSummaries,
  approvalsView,
  onCreatePod,
  onSelectPod,
  onSelectChat,
}: {
  workspace: workspaces.WorkspaceName;
  pods: readonly pods.Pod[];
  chats: readonly MobileChatSummary[];
  podChangeSummaries: ReadonlyMap<pods.PodId, PodChangeSummary>;
  approvalsView: ReactNode;
  onCreatePod: () => void;
  onSelectPod: (podId: pods.PodId) => void;
  onSelectChat: (podId: pods.PodId, chatId: chats.ChatId) => void;
}) {
  const podTitles = useMemo(
    () => new Map(workspacePods.map((pod) => [pod.id, pod.title || "Untitled task"])),
    [workspacePods],
  );
  const attentionChats = workspaceChats.filter(
    (chat) => chat.attention || chat.status === "needs-input" || chat.status === "failed",
  );
  const activeChats = workspaceChats.filter(
    (chat) => chat.status === "working" && !attentionChats.includes(chat),
  );
  const recentChats = workspaceChats
    .filter((chat) => !attentionChats.includes(chat) && !activeChats.includes(chat))
    .toSorted((left, right) => right.updatedAt.localeCompare(left.updatedAt))
    .slice(0, RECENT_CHAT_LIMIT);

  return (
    <div className="mobile-client-content min-h-0 flex-1 overflow-y-auto pt-4">
      <div className="mx-auto grid max-w-2xl gap-6">
        <Button className="h-12 w-full rounded-xl text-sm" variant="primary" onClick={onCreatePod}>
          <Plus aria-hidden="true" className="size-4" />
          Start a New Task
        </Button>

        {approvalsView}

        {attentionChats.length ? (
          <MobileChatSection
            title="Needs Attention"
            chats={attentionChats}
            podTitles={podTitles}
            onSelect={onSelectChat}
          />
        ) : null}

        {activeChats.length ? (
          <MobileChatSection
            title="Working"
            chats={activeChats}
            podTitles={podTitles}
            onSelect={onSelectChat}
          />
        ) : null}

        {recentChats.length ? (
          <MobileChatSection
            title="Recent Chats"
            chats={recentChats}
            podTitles={podTitles}
            onSelect={onSelectChat}
          />
        ) : null}

        <section aria-labelledby="mobile-pods-title">
          <MobileSectionHeading
            id="mobile-pods-title"
            title={`Tasks in ${workspace}`}
            count={workspacePods.length}
          />
          <div className="mt-3 grid gap-2">
            {workspacePods.map((pod) => (
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
                No tasks yet. Start one with a prompt above.
              </div>
            ) : null}
          </div>
        </section>
      </div>
    </div>
  );
}

const RECENT_CHAT_LIMIT = 8;
