import { useEffect, useMemo, useState } from "react";

import type { chats } from "../../../api/generated/index.ts";
import {
  chatTimeline,
  chatTurns,
  type ChatReplica,
} from "../model/replicas.ts";
import {
  presentChatLineChanges,
  type LineChangePresentation,
} from "../model/fileChanges.ts";
import {
  presentContextUsage,
  type ContextUsagePresentation,
} from "../model/contextUsage.ts";
import { presentChatUsage, type UsagePresentation } from "../model/usage.ts";

export type ChatStatusVariant = "inline" | "dock" | "quiet";

export function ChatStatusBar({
  summary,
  replica,
  runningTurn,
  variant = "inline",
}: {
  summary: chats.ChatSummary;
  replica?: ChatReplica;
  runningTurn?: chats.ChatTurn;
  variant?: ChatStatusVariant;
}) {
  const [now, setNow] = useState(Date.now);
  const turns = useMemo(
    () => replica ? chatTurns(replica) : [],
    [replica?.turnOrder, replica?.turnsById],
  );
  const timeline = useMemo(
    () => replica ? chatTimeline(replica) : [],
    [replica?.timelineOrder, replica?.timelineById],
  );
  const status = chatStatus(summary, replica, turns, timeline, runningTurn, now);
  const totalUsage = useMemo(
    () => replica ? presentChatUsage(turns) : undefined,
    [replica !== undefined, turns],
  );
  const contextUsage = useMemo(
    () => presentContextUsage(summary.contextUsage),
    [summary.contextUsage],
  );
  const lineChanges = useMemo(
    () => replica ? presentChatLineChanges(timeline) : undefined,
    [replica !== undefined, timeline],
  );

  useEffect(() => {
    if (!runningTurn) return;
    setNow(Date.now());
    const interval = window.setInterval(() => setNow(Date.now()), 1_000);
    return () => window.clearInterval(interval);
  }, [runningTurn?.turnId]);

  if (variant === "inline") {
    return (
      <div
        className="flex min-h-9 w-full flex-wrap items-center justify-between gap-x-4 gap-y-1 px-3.5 py-2 text-xs"
        role="status"
      >
        <StatusLabel status={status} />
        <span className="flex min-w-0 flex-wrap items-center justify-end gap-x-3 gap-y-1 text-right text-muted">
          {status.detail ? <span>{status.detail}</span> : null}
          <ChatMetrics
            contextUsage={contextUsage}
            usage={totalUsage}
            lineChanges={lineChanges}
          />
        </span>
      </div>
    );
  }

  return (
    <div
      className={`relative flex min-h-11 flex-wrap items-center justify-between gap-x-5 gap-y-1 overflow-hidden px-3.5 py-2 text-xs ${
        variant === "dock" ? "border-b border-ui-border" : ""
      }`}
      role="status"
    >
      <span className="flex min-w-0 flex-wrap items-center gap-x-3 gap-y-1">
        <StatusLabel status={status} descriptive />
        {descriptiveDetail(status) ? (
          <span className="text-muted">{descriptiveDetail(status)}</span>
        ) : null}
      </span>
      <ChatMetrics
        contextUsage={contextUsage}
        usage={totalUsage}
        lineChanges={lineChanges}
      />
    </div>
  );
}

function StatusLabel({ status, descriptive = false }: { status: Status; descriptive?: boolean }) {
  const label = descriptive
    ? status.label === "Working"
      ? "Agent is working"
      : status.label === "Detached"
        ? "Ready"
        : status.label
    : status.label;
  return (
    <span
      className={`inline-flex shrink-0 items-center gap-2 font-medium ${toneClasses[status.tone]}`}
    >
      {status.busy ? <Spinner /> : null}
      {label}
      {status.leadingDetail ? (
        <span className="tabular-nums text-muted">{status.leadingDetail}</span>
      ) : null}
    </span>
  );
}

function descriptiveDetail(status: Status): string | undefined {
  return status.label === "Detached" ? "Reconnects when you send" : status.detail;
}

function ChatMetrics({
  contextUsage,
  usage,
  lineChanges,
}: {
  contextUsage: ContextUsagePresentation;
  usage?: UsagePresentation;
  lineChanges?: LineChangePresentation;
}) {
  const description = [contextUsage.description, usage?.description, lineChanges?.description]
    .filter((part): part is string => part !== undefined)
    .join(" · ");
  return (
    <span
      className="flex shrink-0 flex-wrap items-center gap-x-3 gap-y-1 tabular-nums text-muted"
      title={description}
    >
      <span><span className="text-subtle">Context </span>{contextUsage.value}</span>
      {usage ? (
        <>
          <span><span className="text-subtle">Tokens </span>{usage.total}</span>
          <span><span className="text-subtle">Cost </span>{usage.cost ?? "Not priced"}</span>
        </>
      ) : null}
      {lineChanges ? (
        <span>
          <span className="text-subtle">Lines </span>
          <span className="text-emerald-400/80">+{lineChanges.additions}</span>
          {" "}
          <span className="text-red-300/80">−{lineChanges.deletions}</span>
        </span>
      ) : null}
    </span>
  );
}

type StatusTone = "default" | "active" | "success" | "warning" | "error";

interface Status {
  label: string;
  leadingDetail?: string;
  detail?: string;
  busy?: boolean;
  tone: StatusTone;
}

const toneClasses: Record<StatusTone, string> = {
  default: "text-muted",
  active: "text-accent-text",
  success: "text-emerald-400",
  warning: "text-amber-300",
  error: "text-red-300",
};

function chatStatus(
  summary: chats.ChatSummary,
  replica: ChatReplica | undefined,
  turns: chats.ChatTurn[],
  timeline: chats.ChatTimelineEntry[],
  runningTurn: chats.ChatTurn | undefined,
  now: number,
): Status {
  if (summary.lastBindingError) {
    return {
      label: "Attachment failed",
      detail: `${summary.lastBindingError.code}: ${summary.lastBindingError.message}`,
      tone: "error",
    };
  }
  if (!replica) {
    return { label: "Loading", busy: true, tone: "active" };
  }
  if (runningTurn) {
    return {
      label: "Working",
      leadingDetail: elapsedTime(runningTurn, now),
      busy: true,
      tone: "active",
    };
  }

  const currentBindingId = summary.binding?.bindingId;
  const waitingForInput = timeline.some(
    (entry) =>
      entry.entry === "Request"
      && !entry.resolved
      && entry.bindingId === currentBindingId,
  );
  if (waitingForInput) {
    return {
      label: "Waiting",
      detail: "Answer the request above to continue",
      tone: "warning",
    };
  }

  switch (summary.binding?.status) {
    case "Attaching":
      return {
        label: "Connecting",
        detail: queuedPromptDetail(replica),
        busy: true,
        tone: "active",
      };
    case "Detaching":
      return { label: "Disconnecting", busy: true, tone: "warning" };
    case "Attached": {
      const lastTurn = turns.findLast((turn) => turn.state !== "Running");
      return {
        label: "Done",
        leadingDetail: lastTurn ? elapsedTime(lastTurn, now) : undefined,
        tone: "success",
      };
    }
    case undefined:
      return {
        label: "Detached",
        detail: "Send a message to reconnect",
        tone: "default",
      };
  }
}

function Spinner() {
  return (
    <span
      className="size-3 animate-spin rounded-full border-2 border-current/20 border-t-current"
      aria-hidden="true"
    />
  );
}

function queuedPromptDetail(replica: ChatReplica): string | undefined {
  const count = replica.queuedPrompts.length;
  return count === 0 ? undefined : `${count} queued ${count === 1 ? "prompt" : "prompts"}`;
}

function elapsedTime(turn: chats.ChatTurn, now: number): string | undefined {
  if (turn.startedAt === undefined) return undefined;
  const startedAt = new Date(String(turn.startedAt)).getTime();
  if (Number.isNaN(startedAt)) return undefined;

  const completedAt =
    turn.completedAt === undefined
      ? now
      : new Date(String(turn.completedAt)).getTime();
  if (Number.isNaN(completedAt)) return undefined;

  const totalSeconds = Math.floor(Math.max(0, completedAt - startedAt) / 1_000);
  const seconds = totalSeconds % 60;
  const totalMinutes = Math.floor(totalSeconds / 60);
  const minutes = totalMinutes % 60;
  const hours = Math.floor(totalMinutes / 60);
  return hours === 0
    ? `${minutes}:${seconds.toString().padStart(2, "0")}`
    : `${hours}:${minutes.toString().padStart(2, "0")}:${seconds.toString().padStart(2, "0")}`;
}
