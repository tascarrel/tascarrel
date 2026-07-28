import {
  AlertTriangle,
  ArrowRight,
  Bell,
  Bot,
  LoaderCircle,
} from "lucide-react";

import type { chats, pods } from "../../../api/generated/index.ts";
import { CountBadge } from "../../../components/ui/CountBadge.tsx";
import { relativeTime } from "../../chat/model/format.ts";
import type { PodChangeSummary } from "../../changes/podChangeSummary.ts";

export type MobileChatSummary = {
  id: chats.ChatId;
  podId: pods.PodId;
  title: string;
  status?: "working" | "needs-input" | "failed" | "connected";
  attention: boolean;
  updatedAt: string;
};

export function mobileChatSummary(summary: chats.ChatSummary): MobileChatSummary {
  return {
    id: summary.chatId,
    podId: summary.podId,
    title: summary.title || "Untitled chat",
    status: chatStatus(summary),
    attention: summary.attentionRequired,
    updatedAt: String(summary.updatedAt),
  };
}

export function MobileChatRow({
  chat,
  podTitle,
  onClick,
}: {
  chat: MobileChatSummary;
  podTitle?: string;
  onClick: () => void;
}) {
  return (
    <button
      className={`flex min-h-16 w-full min-w-0 max-w-full items-center gap-3 overflow-hidden rounded-2xl border p-3.5 text-left transition active:bg-surface-raised ${
        chat.status === "needs-input" || chat.attention
          ? "border-amber-500/25 bg-amber-500/[0.05]"
          : chat.status === "failed"
            ? "border-red-500/25 bg-red-500/[0.04]"
            : "border-ui-border bg-surface/70"
      }`}
      type="button"
      onClick={onClick}
    >
      <ChatStatusIcon chat={chat} />
      <span className="min-w-0 flex-1">
        <span className="block truncate text-sm font-medium text-foreground">
          {chat.title}
        </span>
        <span className="mt-1 block truncate text-[11px] text-subtle">
          {[podTitle, chatStatusLabel(chat), relativeTime(chat.updatedAt)]
            .filter(Boolean)
            .join(" · ")}
        </span>
      </span>
      <ArrowRight aria-hidden="true" className="size-4 shrink-0 text-subtle" />
    </button>
  );
}

export function MobilePodRow({
  pod,
  changeSummary,
  attention,
  working,
  onClick,
}: {
  pod: pods.Pod;
  changeSummary?: PodChangeSummary;
  attention: boolean;
  working: boolean;
  onClick: () => void;
}) {
  return (
    <button
      className="flex min-h-16 w-full min-w-0 max-w-full items-center gap-3 overflow-hidden rounded-2xl border border-ui-border bg-surface/70 p-3.5 text-left transition active:bg-surface-raised"
      type="button"
      onClick={onClick}
    >
      <span className="grid size-9 shrink-0 place-items-center rounded-xl bg-surface-raised text-muted">
        {attention ? (
          <Bell aria-label="Needs attention" className="size-4 text-amber-300" />
        ) : working ? (
          <LoaderCircle aria-label="Chat working" className="size-4 animate-spin text-accent-text" />
        ) : (
          <Bot aria-hidden="true" className="size-4" />
        )}
      </span>
      <span className="min-w-0 flex-1">
        <span className="block truncate text-sm font-medium text-foreground">
          {pod.title || "Untitled task"}
        </span>
        <span className="mt-1 block truncate text-[11px] text-subtle">
          {pod.status.status}
          {changeSummary?.changedFileCount
            ? ` · ${changeSummary.changedFileCount} changed`
            : changeSummary?.unpushedCommitCount
              ? ` · ${changeSummary.unpushedCommitCount} unpushed`
              : ""}
        </span>
      </span>
      <ArrowRight aria-hidden="true" className="size-4 shrink-0 text-subtle" />
    </button>
  );
}

export function MobileSectionHeading({
  id,
  title,
  count,
}: {
  id: string;
  title: string;
  count: number;
}) {
  return (
    <h2
      className="flex min-w-0 flex-1 items-center gap-2 text-xs font-semibold uppercase tracking-[0.08em] text-muted"
      id={id}
    >
      <span className="truncate">{title}</span>
      <CountBadge count={count} size="xs" tone="muted" />
    </h2>
  );
}

function ChatStatusIcon({ chat }: { chat: MobileChatSummary }) {
  if (chat.status === "working") {
    return (
      <span className="grid size-9 shrink-0 place-items-center rounded-xl bg-accent/10 text-accent-text">
        <LoaderCircle aria-label="Working" className="size-4 animate-spin" />
      </span>
    );
  }
  if (chat.status === "needs-input" || chat.attention) {
    return (
      <span className="grid size-9 shrink-0 place-items-center rounded-xl bg-amber-500/10 text-amber-300">
        <Bell aria-label="Needs attention" className="size-4" />
      </span>
    );
  }
  if (chat.status === "failed") {
    return (
      <span className="grid size-9 shrink-0 place-items-center rounded-xl bg-red-500/10 text-red-300">
        <AlertTriangle aria-label="Failed" className="size-4" />
      </span>
    );
  }
  return (
    <span className="grid size-9 shrink-0 place-items-center rounded-xl bg-surface-raised text-muted">
      <Bot aria-hidden="true" className="size-4" />
    </span>
  );
}

function chatStatus(summary: chats.ChatSummary): MobileChatSummary["status"] {
  if (summary.agentStatus === "Working") return "working";
  if (summary.agentStatus === "UserInputRequired") return "needs-input";
  if (summary.lastBindingError) return "failed";
  if (summary.binding?.status === "Attached") return "connected";
  return undefined;
}

function chatStatusLabel(chat: MobileChatSummary): string {
  if (chat.status === "working") return "Working";
  if (chat.status === "needs-input") return "Needs input";
  if (chat.status === "failed") return "Failed";
  if (chat.attention) return "Needs attention";
  return "Idle";
}
