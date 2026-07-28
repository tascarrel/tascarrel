import { FileDiff, LoaderCircle, Plus, Square, Trash2 } from "lucide-react";

import type { chats, pods } from "../../../api/generated/index.ts";
import { Badge, type BadgeTone } from "../../../components/ui/Badge.tsx";
import { Button } from "../../../components/ui/Button.tsx";
import type { PodChangeSummary } from "../../changes/podChangeSummary.ts";
import {
  MobileChatRow,
  MobileSectionHeading,
  type MobileChatSummary,
} from "./MobilePodList.tsx";

export function MobilePodScreen({
  pod,
  chats: podChats,
  changeSummary,
  pendingOperation,
  actionError,
  onSelectChat,
  onNewChat,
  onOpenChanges,
  onStart,
  onStop,
  onDelete,
}: {
  pod: pods.Pod;
  chats: readonly MobileChatSummary[];
  changeSummary?: PodChangeSummary;
  pendingOperation?: "start" | "stop" | "destroy";
  actionError?: string;
  onSelectChat: (chatId: chats.ChatId) => void;
  onNewChat: () => void;
  onOpenChanges: () => void;
  onStart: () => void;
  onStop: () => void;
  onDelete: () => void;
}) {
  const pending = pendingOperation !== undefined;

  return (
    <div className="mobile-client-content min-h-0 flex-1 overflow-y-auto pt-4">
      <div className="mx-auto grid w-full min-w-0 max-w-2xl grid-cols-[minmax(0,1fr)] gap-6">
        <section className="min-w-0 max-w-full overflow-hidden rounded-2xl border border-ui-border bg-surface/70 p-4">
          <div className="flex items-start justify-between gap-3">
            <div className="min-w-0">
              <h2 className="truncate text-base font-semibold text-foreground">
                {pod.title || "Untitled pod"}
              </h2>
              <p className="mt-1 break-all font-mono text-[10px] text-subtle">{pod.id}</p>
            </div>
            <Badge className="shrink-0" tone={podStatusTone(pod.status.status)}>
              {pod.status.status}
            </Badge>
          </div>
          {pod.status.status === "Failed" ? (
            <p
              className="mt-3 [overflow-wrap:anywhere] rounded-xl border border-red-500/20 bg-red-500/5 p-3 text-xs leading-5 text-red-200"
              role="alert"
            >
              {pod.status.message}
            </p>
          ) : null}
          {actionError ? (
            <p className="mt-3 [overflow-wrap:anywhere] text-xs leading-5 text-red-200" role="alert">
              {actionError}
            </p>
          ) : null}
          <div className="mt-4 flex flex-wrap gap-2">
            {(pod.status.status === "Stopped" || pod.status.status === "Failed") ? (
              <Button className="h-11 flex-1" disabled={pending} onClick={onStart}>
                {pendingOperation === "start"
                  ? <LoaderCircle aria-hidden="true" className="size-4 animate-spin" />
                  : null}
                Start
              </Button>
            ) : null}
            {pod.status.status === "Running" ? (
              <Button className="h-11 flex-1" disabled={pending} onClick={onStop}>
                {pendingOperation === "stop"
                  ? <LoaderCircle aria-hidden="true" className="size-4 animate-spin" />
                  : <Square aria-hidden="true" className="size-3.5" />}
                Stop
              </Button>
            ) : null}
            {pod.status.status !== "Destroying" ? (
              <Button
                className="h-11 flex-1"
                variant="danger"
                disabled={pending}
                onClick={onDelete}
              >
                {pendingOperation === "destroy"
                  ? <LoaderCircle aria-hidden="true" className="size-4 animate-spin" />
                  : <Trash2 aria-hidden="true" className="size-4" />}
                Delete Pod
              </Button>
            ) : null}
          </div>
        </section>

        <section className="min-w-0 max-w-full" aria-labelledby="mobile-chats-title">
          <div className="flex items-center justify-between gap-3">
            <MobileSectionHeading id="mobile-chats-title" title="Chats" count={podChats.length} />
            <Button
              className="h-11"
              variant="primary"
              disabled={pod.status.status !== "Running"}
              onClick={onNewChat}
            >
              <Plus aria-hidden="true" className="size-4" />
              New Chat
            </Button>
          </div>
          <div className="mt-3 grid gap-2">
            {podChats
              .toSorted((left, right) => right.updatedAt.localeCompare(left.updatedAt))
              .map((chat) => (
                <MobileChatRow
                  key={chat.id}
                  chat={chat}
                  onClick={() => onSelectChat(chat.id)}
                />
              ))}
            {!podChats.length ? (
              <div className="rounded-2xl border border-dashed border-ui-border p-6 text-center text-sm leading-6 text-subtle">
                This pod has no chats yet.
              </div>
            ) : null}
          </div>
        </section>

        {changeSummary
          && (changeSummary.changedFileCount > 0 || changeSummary.unpushedCommitCount > 0) ? (
          <Button
            className="min-h-16 w-full min-w-0 max-w-full overflow-hidden justify-start rounded-2xl p-4 text-left"
            onClick={onOpenChanges}
          >
            <FileDiff aria-hidden="true" className="size-5 shrink-0 text-accent-text" />
            <span className="min-w-0 flex-1">
              <span className="block text-sm font-medium text-foreground">Review Changed Files</span>
              <span className="mt-0.5 block text-xs font-normal text-subtle">
                {changeSummary.changedFileCount > 0
                  ? `${changeSummary.changedFileCount} changed`
                  : `${changeSummary.unpushedCommitCount} unpushed commits`}
                {changeSummary.conflictCount > 0 ? ` · ${changeSummary.conflictCount} conflicts` : ""}
              </span>
            </span>
          </Button>
        ) : null}
      </div>
    </div>
  );
}

function podStatusTone(status: pods.PodState["status"]): BadgeTone {
  if (status === "Running") return "success";
  if (status === "Failed") return "danger";
  if (
    status === "Creating"
    || status === "Building"
    || status === "Starting"
    || status === "Initializing"
    || status === "Stopping"
    || status === "Destroying"
  ) return "warning";
  return "muted";
}
