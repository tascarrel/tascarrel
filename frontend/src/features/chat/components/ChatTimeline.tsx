import { ChevronRight } from "lucide-react";
import {
  useCallback,
  useEffect,
  useLayoutEffect,
  memo,
  useMemo,
  useRef,
  useState,
} from "react";

import type { chats } from "../../../api/generated/index.ts";
import { Badge } from "../../../components/ui/Badge.tsx";
import { Button } from "../../../components/ui/Button.tsx";
import {
  findFilePatches,
  findStructuredFilePatches,
} from "../model/fileChanges.ts";
import { formatBytes, formatTime, prettyJson } from "../model/format.ts";
import { type ChatReplica, timelineEntryKey } from "../model/replicas.ts";
import type { AttachmentUrlResolver } from "../types.ts";
import { AttachmentPreview } from "./AttachmentPreview.tsx";
import { ChatStatusBar } from "./ChatStatusBar.tsx";
import { DiffViewer } from "../../../components/ui/DiffViewer.tsx";
import { HighlightedCode, MarkdownContent } from "./MarkdownContent.tsx";
import { QuestionRequest } from "./QuestionRequest.tsx";
import { StructuredItemContent } from "./StructuredItemContent.tsx";

const INITIAL_TIMELINE_NODES = 40;
const TIMELINE_NODE_CHUNK = 40;

export function ChatTimeline({
  entries,
  replica,
  summary,
  runningTurn,
  resolvingRequestId,
  onResolveRequest,
  attachmentUrl,
  showUnknownItems = false,
  showStatus = true,
  initialFollowing = true,
  onFollowingChange,
}: {
  entries: chats.ChatTimelineEntry[];
  replica: ChatReplica;
  summary: chats.ChatSummary;
  runningTurn?: chats.ChatTurn;
  resolvingRequestId?: chats.ChatRequestId;
  onResolveRequest: (
    requestId: chats.ChatRequestId,
    answers: chats.ChatQuestionAnswer[],
  ) => Promise<void>;
  attachmentUrl?: AttachmentUrlResolver;
  showUnknownItems?: boolean;
  showStatus?: boolean;
  initialFollowing?: boolean;
  onFollowingChange?: (following: boolean) => void;
}) {
  const end = useRef<HTMLDivElement>(null);
  const olderEntries = useRef<HTMLDivElement>(null);
  const following = useRef(initialFollowing);
  const reportedFollowing = useRef(initialFollowing);
  const prependAnchor = useRef<{
    scroll: HTMLElement;
    scrollHeight: number;
    scrollTop: number;
  } | undefined>(undefined);
  const prependPending = useRef(false);
  const timeline = useMemo(
    () =>
      groupTimelineEntries(
        entries.filter(
          (entry) =>
            !isEmptyReasoning(entry)
            && (showUnknownItems
              || (!(entry.entry === "Item" && entry.kind === "Unknown")
                && !(entry.entry === "Activity" && isUnknownActivity(entry)))),
        ),
      ),
    [entries, showUnknownItems],
  );
  const [firstVisibleNode, setFirstVisibleNode] = useState(() =>
    Math.max(0, timeline.length - INITIAL_TIMELINE_NODES),
  );
  const firstVisibleNodeRef = useRef(firstVisibleNode);
  firstVisibleNodeRef.current = firstVisibleNode;
  const visibleTimeline = timeline.slice(firstVisibleNode);
  const hasOlderEntries = firstVisibleNode > 0;

  const revealOlderEntries = useCallback(() => {
    if (prependPending.current || firstVisibleNodeRef.current === 0) return;
    const scroll = olderEntries.current?.closest<HTMLElement>("[data-chat-scroll]");
    if (!scroll) return;

    prependPending.current = true;
    prependAnchor.current = {
      scroll,
      scrollHeight: scroll.scrollHeight,
      scrollTop: scroll.scrollTop,
    };
    const next = Math.max(0, firstVisibleNodeRef.current - TIMELINE_NODE_CHUNK);
    firstVisibleNodeRef.current = next;
    setFirstVisibleNode(next);
  }, []);

  useLayoutEffect(() => {
    const anchor = prependAnchor.current;
    if (!anchor) return;
    anchor.scroll.scrollTop =
      anchor.scrollTop + (anchor.scroll.scrollHeight - anchor.scrollHeight);
    prependAnchor.current = undefined;
    prependPending.current = false;
  }, [firstVisibleNode]);

  useEffect(() => {
    if (!hasOlderEntries) return;
    const marker = olderEntries.current;
    const scroll = marker?.closest<HTMLElement>("[data-chat-scroll]");
    if (!marker || !scroll) return;

    const observer = new IntersectionObserver(
      ([entry]) => {
        if (entry?.isIntersecting) revealOlderEntries();
      },
      { root: scroll, rootMargin: "160px 0px 0px" },
    );
    observer.observe(marker);
    return () => observer.disconnect();
  }, [hasOlderEntries, revealOlderEntries]);

  useEffect(() => {
    const scroll = end.current?.closest<HTMLElement>("[data-chat-scroll]");
    if (!scroll) return;
    const update = () => {
      const nextFollowing = scroll.scrollHeight - scroll.scrollTop - scroll.clientHeight < 120;
      following.current = nextFollowing;
      if (reportedFollowing.current !== nextFollowing) {
        reportedFollowing.current = nextFollowing;
        onFollowingChange?.(nextFollowing);
      }
    };
    scroll.addEventListener("scroll", update, { passive: true });
    return () => scroll.removeEventListener("scroll", update);
  }, [onFollowingChange]);

  useEffect(() => {
    if (!following.current) return;
    const scroll = end.current?.closest<HTMLElement>("[data-chat-scroll]");
    if (!scroll) return;
    scroll.scrollTop = scroll.scrollHeight;
  }, [entries]);

  return (
    <div
      className="mx-auto flex min-h-0 w-full max-w-4xl flex-1 flex-col px-4 pb-16 pt-6 sm:px-6 sm:pt-8"
      role="log"
      aria-label="Chat timeline"
      aria-live="polite"
      aria-relevant="additions"
    >
      {timeline.length === 0 ? (
        <div className="grid flex-1 place-items-center p-8 text-center">
          <div>
            <h2 className="text-base font-semibold text-foreground">Ready for a Task</h2>
            <p className="mt-1.5 text-sm leading-6 text-muted">
              Send a message below. Detached chats reconnect automatically.
            </p>
          </div>
        </div>
      ) : (
        <>
          {hasOlderEntries ? (
            <div className="mb-5 flex justify-center" ref={olderEntries}>
              <Button
                className="h-auto rounded-full px-3 py-1.5"
                size="small"
                onClick={revealOlderEntries}
              >
                Show earlier activity
              </Button>
            </div>
          ) : null}
          {visibleTimeline.map((node) =>
            node.kind === "AgentWork" ? (
              <AgentWorkGroup
                key={node.key}
                entries={node.entries}
                summary={summary}
                attachmentUrl={attachmentUrl}
                onResolveRequest={onResolveRequest}
              />
            ) : (
              <TimelineEntry
                key={timelineEntryKey(node.entry)}
                entry={node.entry}
                summary={summary}
                attachmentUrl={attachmentUrl}
                resolving={
                  node.entry.entry === "Request" && node.entry.requestId === resolvingRequestId
                }
                onResolveRequest={onResolveRequest}
                showUnknownItems={showUnknownItems}
              />
            ),
          )}
        </>
      )}
      {showStatus ? (
        <div className="mt-auto pt-4">
          <ChatStatusBar summary={summary} replica={replica} runningTurn={runningTurn} />
        </div>
      ) : null}
      <div ref={end} />
    </div>
  );
}

function AgentWorkGroup({
  entries,
  summary,
  attachmentUrl,
  onResolveRequest,
}: {
  entries: ChatItemEntry[];
  summary: chats.ChatSummary;
  attachmentUrl?: AttachmentUrlResolver;
  onResolveRequest: (
    requestId: chats.ChatRequestId,
    answers: chats.ChatQuestionAnswer[],
  ) => Promise<void>;
}) {
  const counts = workItemCounts(entries, summary);
  const [open, setOpen] = useState(false);

  return (
    <details
      className="group/work mb-5 overflow-hidden rounded-xl border border-ui-border bg-surface"
      open={open}
      onToggle={(event) => setOpen(event.currentTarget.open)}
    >
      <summary className="flex cursor-pointer list-none items-center gap-2 px-3.5 py-2.5 text-xs text-muted transition hover:bg-surface-raised [&::-webkit-details-marker]:hidden">
        <ChevronRight className="size-3.5 transition group-open/work:rotate-90" />
        <span className="font-medium text-foreground">Agent work</span>
        <span className="text-subtle">{counts.join(", ")}</span>
      </summary>
      {open ? (
        <div className="divide-y divide-ui-border border-t border-ui-border [&>details]:mb-0 [&>details]:rounded-none [&>details]:border-0">
          {entries.map((entry) => (
            <TimelineEntry
              key={entry.itemId}
              entry={entry}
              summary={summary}
              attachmentUrl={attachmentUrl}
              resolving={false}
              onResolveRequest={onResolveRequest}
              showUnknownItems={false}
            />
          ))}
        </div>
      ) : null}
    </details>
  );
}

function TimelineEntry({
  entry,
  summary,
  attachmentUrl,
  resolving,
  onResolveRequest,
  showUnknownItems,
}: {
  entry: chats.ChatTimelineEntry;
  summary: chats.ChatSummary;
  attachmentUrl?: AttachmentUrlResolver;
  resolving: boolean;
  onResolveRequest: (
    requestId: chats.ChatRequestId,
    answers: chats.ChatQuestionAnswer[],
  ) => Promise<void>;
  showUnknownItems: boolean;
}) {
  if (entry.entry === "Activity") {
    if (!showUnknownItems && isUnknownActivity(entry)) return null;
    return <ActivityEntry activity={entry} />;
  }
  if (entry.entry === "Request") {
    const current =
      summary.binding?.status === "Attached" && summary.binding.bindingId === entry.bindingId;
    return (
      <QuestionRequest
        request={entry}
        enabled={current && !entry.resolved}
        resolving={resolving}
        onResolve={onResolveRequest}
      />
    );
  }
  if (entry.kind === "Unknown" && !showUnknownItems) return null;

  const state = displayItemState(entry, summary);

  switch (entry.kind) {
    case "UserMessage":
      return <MessageEntry attachmentUrl={attachmentUrl} item={entry} state={state} role="user" />;
    case "AssistantMessage":
      return <MessageEntry attachmentUrl={attachmentUrl} item={entry} state={state} role="assistant" />;
    case "Reasoning":
      return <DetailEntry attachmentUrl={attachmentUrl} item={entry} state={state} label="Reasoning" defaultOpen={state === "Started"} />;
    case "Plan":
      return <DetailEntry attachmentUrl={attachmentUrl} item={entry} state={state} label="Plan" defaultOpen />;
    case "CommandExecution":
      return (
        <DetailEntry
          item={entry}
          state={state}
          attachmentUrl={attachmentUrl}
          label="Command"
          defaultOpen={state === "Started" || state === "Failed"}
        />
      );
    case "FileChange":
      return (
        <DetailEntry
          item={entry}
          state={state}
          attachmentUrl={attachmentUrl}
          label="File changes"
          defaultOpen={state === "Started" || state === "Failed"}
        />
      );
    case "ToolCall":
      return (
        <DetailEntry
          item={entry}
          state={state}
          attachmentUrl={attachmentUrl}
          label="Tool call"
          defaultOpen={state === "Started" || state === "Failed"}
        />
      );
    case "WebSearch":
      return <DetailEntry attachmentUrl={attachmentUrl} item={entry} state={state} label="Web search" />;
    case "Subagent":
      return (
        <DetailEntry
          item={entry}
          state={state}
          attachmentUrl={attachmentUrl}
          label="Delegated work"
          defaultOpen={state === "Started"}
        />
      );
    case "ContextCompaction":
      return <ContextCompactionEntry state={state} />;
    case "Error":
      return <DetailEntry attachmentUrl={attachmentUrl} item={entry} state={state} label="Error" defaultOpen tone="error" />;
    case "Unknown":
      return <DetailEntry attachmentUrl={attachmentUrl} item={entry} state={state} label="Unknown item" />;
  }
}

function ContextCompactionEntry({ state }: { state: chats.ChatItemState }) {
  return (
    <div className="mx-auto my-4 w-4/5 border-y border-ui-border py-2 text-center text-xs text-subtle">
      {state === "Started"
        ? "Compacting context…"
        : state === "Failed"
          ? "Context compaction failed"
          : "Context compacted"}
    </div>
  );
}

function MessageEntry({
  item,
  state,
  role,
  attachmentUrl,
}: {
  item: chats.ChatItem;
  state: chats.ChatItemState;
  role: "user" | "assistant";
  attachmentUrl?: AttachmentUrlResolver;
}) {
  const assistant = role === "assistant";
  return (
    <article className={`mb-7 flex ${assistant ? "" : "justify-end"}`}>
      <div className={`min-w-0 ${assistant ? "w-full" : "max-w-[85%]"}`}>
        <div className={`mb-1 flex items-center gap-2 text-xs ${assistant ? "" : "justify-end"}`}>
          <span className="font-semibold text-muted">{assistant ? "Agent" : "You"}</span>
          {assistant && state === "Started" ? null : <ItemState state={state} />}
        </div>
        <div
          className={
            assistant
              ? "min-w-0"
              : "rounded-2xl rounded-tr-sm border border-ui-border bg-surface-raised px-4 py-2.5"
          }
        >
          <ItemContent attachmentUrl={attachmentUrl} item={item} />
        </div>
      </div>
    </article>
  );
}

function DetailEntry({
  item,
  state,
  label,
  defaultOpen = false,
  tone = "default",
  attachmentUrl,
}: {
  item: chats.ChatItem;
  state: chats.ChatItemState;
  label: string;
  defaultOpen?: boolean;
  tone?: "default" | "error";
  attachmentUrl?: AttachmentUrlResolver;
}) {
  const preview = itemPreview(item);
  const [open, setOpen] = useState(defaultOpen);

  return (
    <details
      className={`group mb-5 overflow-hidden rounded-xl border ${
        tone === "error" ? "border-red-500/25 bg-red-500/5" : "border-ui-border bg-surface"
      }`}
      open={open}
      onToggle={(event) => setOpen(event.currentTarget.open)}
    >
      <summary className="flex cursor-pointer list-none items-center gap-2 px-3.5 py-2.5 text-xs text-muted transition hover:bg-surface-raised [&::-webkit-details-marker]:hidden">
        <ChevronRight className="size-3.5 transition group-open:rotate-90" />
        <span className="shrink-0 font-medium text-foreground">{label}</span>
        <ItemState state={state} />
        {preview ? (
          <span className="min-w-0 flex-1 truncate text-subtle">{preview}</span>
        ) : null}
      </summary>
      {open ? (
        <div className="border-t border-ui-border px-4 py-3.5">
          <ItemContent attachmentUrl={attachmentUrl} item={item} />
        </div>
      ) : null}
    </details>
  );
}

const ItemContent = memo(function ItemContent({
  item,
  attachmentUrl,
}: {
  item: chats.ChatItem;
  attachmentUrl?: AttachmentUrlResolver;
}) {
  if (item.content.length === 0) {
    return item.state === "Started" ? (
      <div className="flex items-center gap-2 py-1 text-xs text-subtle">
        Waiting for output…
      </div>
    ) : null;
  }

  const patches = item.kind === "FileChange" ? findFilePatches(item.content) : [];
  return (
    <div className="min-w-0 space-y-3 [overflow-wrap:anywhere]">
      {patches.map(({ patch, fileName }, index) => (
        <DiffViewer
          fileName={fileName}
          key={`${fileName ?? "file"}-${index}`}
          patch={patch}
        />
      ))}
      {item.content.map((part, index) => {
        if (part.kind === "Text") {
          if (patches.some(({ patch }) => patch === part.value)) return null;
          if (item.kind === "CommandExecution" && !looksLikeMarkdown(part.value)) {
            return <HighlightedCode code={part.value} language="text" key={index} />;
          }
          return (
            <MarkdownContent
              content={part.value}
              density={item.kind === "Reasoning" ? "compact" : "default"}
              key={index}
            />
          );
        }
        if (part.kind === "Attachment") {
          return (
            <PromptAttachmentFact
              attachment={{
                name: part.name,
                mediaType: part.mediaType,
                size: Number(part.size),
                attachmentId: part.attachmentId,
              }}
              url={attachmentUrl?.(part.attachmentId)}
              key={index}
            />
          );
        }
        if (part.kind === "Structured") {
          const attachment =
            item.kind === "UserMessage" ? structuredChatAttachment(part.value) : undefined;
          if (attachment) {
            return (
              <PromptAttachmentFact
                attachment={attachment}
                key={index}
                url={attachment.attachmentId ? attachmentUrl?.(attachment.attachmentId) : undefined}
              />
            );
          }
          const message =
            item.kind === "UserMessage" || item.kind === "AssistantMessage"
              ? structuredMessageText(part.value)
              : undefined;
          if (message) return <MarkdownContent content={message} key={index} />;
          if (item.kind === "FileChange" && findStructuredFilePatches(part.value).length > 0) return null;
          return <StructuredItemContent kind={item.kind} value={part.value} key={index} />;
        }
        return (
          <div
            className="flex flex-wrap items-center gap-x-3 gap-y-1 rounded-xl border border-ui-border bg-surface-raised px-3 py-2 font-mono text-[10px] text-subtle"
            key={index}
          >
            <span>{part.mediaType}</span>
            <span>{formatBytes(part.size)}</span>
            <span className="truncate">{part.digest}</span>
          </div>
        );
      })}
    </div>
  );
});

interface PromptAttachmentDescriptor {
  attachmentId?: chats.ChatAttachmentId;
  name: string;
  mediaType?: string;
  size?: number;
}

function PromptAttachmentFact({
  attachment,
  url,
}: {
  attachment: PromptAttachmentDescriptor;
  url?: string;
}) {
  return <AttachmentPreview attachment={attachment} url={url} />;
}

function ActivityEntry({ activity }: { activity: chats.ChatActivity }) {
  const tone =
    activity.kind === "Error"
      ? "border-red-500/20 bg-red-500/5 text-red-300"
      : activity.kind === "Warning"
        ? "border-amber-500/20 bg-amber-500/5 text-amber-300"
        : "border-ui-border bg-surface text-muted";
  return (
    <div className={`mx-auto my-4 flex max-w-3xl items-start gap-3 rounded-xl border px-3.5 py-2.5 text-xs ${tone}`}>
      <div className="min-w-0 flex-1">
        <div>{activity.message}</div>
        {activity.detail !== undefined ? (
          <details className="mt-1.5 text-subtle">
            <summary className="cursor-pointer">Details</summary>
            <pre className="mt-2 overflow-x-auto whitespace-pre-wrap font-mono text-[10px]">
              {prettyJson(activity.detail)}
            </pre>
          </details>
        ) : null}
      </div>
      <time className="shrink-0 text-[10px] text-subtle">{formatTime(activity.occurredAt)}</time>
    </div>
  );
}

function ItemState({ state }: { state: chats.ChatItemState }) {
  if (state === "Completed") return null;
  return (
    <Badge size="xs" tone={state === "Failed" ? "danger" : "primary"}>
      {state}
    </Badge>
  );
}

function stringField(record: Record<string, unknown>, ...keys: string[]): string | undefined {
  for (const key of keys) {
    const value = record[key];
    if (typeof value === "string") return value;
  }
  return undefined;
}

function itemPreview(item: chats.ChatItem): string | undefined {
  for (const part of item.content) {
    const preview =
      part.kind === "Text"
        ? part.value
        : part.kind === "Structured"
          ? structuredItemPreview(item.kind, part.value)
          : part.mediaType;
    const compact = compactPreview(preview);
    if (compact) return compact;
  }
  return undefined;
}

function structuredItemPreview(kind: chats.ChatItemKind, value: unknown): string {
  if (!value || typeof value !== "object" || Array.isArray(value)) return prettyJson(value);
  const record = value as Record<string, unknown>;

  switch (kind) {
    case "CommandExecution":
      return stringField(record, "command", "cmd") ?? prettyJson(value);
    case "FileChange":
      return fileChangePreview(record) ?? prettyJson(value);
    case "ToolCall": {
      const tool = stringField(record, "tool", "toolName", "name");
      const server = stringField(record, "server", "serverName");
      return tool && server ? `${tool} via ${server}` : tool ?? server ?? prettyJson(value);
    }
    case "WebSearch":
      return stringField(record, "query", "text") ?? prettyJson(value);
    case "Subagent":
      return stringField(record, "prompt", "message", "tool", "name") ?? prettyJson(value);
    default:
      return stringField(record, "text", "message", "name") ?? prettyJson(value);
  }
}

function fileChangePreview(record: Record<string, unknown>): string | undefined {
  const direct = stringField(record, "path", "filePath", "file");
  if (direct) return direct;
  if (!Array.isArray(record.changes)) return undefined;

  const paths = record.changes.flatMap((change) => {
    if (!change || typeof change !== "object" || Array.isArray(change)) return [];
    const path = stringField(
      change as Record<string, unknown>,
      "path",
      "filePath",
      "file",
    );
    return path ? [path] : [];
  });
  if (paths.length === 0) return undefined;
  return paths.length === 1 ? paths[0] : `${paths[0]} and ${paths.length - 1} more`;
}

function compactPreview(value: string): string {
  return value.replace(/\s+/g, " ").trim();
}

function looksLikeMarkdown(value: string): boolean {
  return /(^|\n)(#{1,6} |[-*+] |\d+\. |```|> )|\[[^\]]+\]\([^)]+\)|[*_]{1,2}\S/m.test(value);
}

type TimelineNode =
  | { kind: "Entry"; entry: chats.ChatTimelineEntry }
  | { kind: "AgentWork"; key: string; entries: ChatItemEntry[] };

type ChatItemEntry = Extract<chats.ChatTimelineEntry, { entry: "Item" }>;

function groupTimelineEntries(entries: chats.ChatTimelineEntry[]): TimelineNode[] {
  const nodes: TimelineNode[] = [];
  let work: ChatItemEntry[] = [];
  const flush = () => {
    if (work.length > 0) {
      nodes.push({
        kind: "AgentWork",
        key: `work-${work[0].itemId}`,
        entries: work,
      });
    }
    work = [];
  };

  for (const entry of entries) {
    if (entry.entry === "Item" && isAgentWorkItem(entry.kind)) {
      work.push(entry);
    } else {
      flush();
      nodes.push({ kind: "Entry", entry });
    }
  }
  flush();
  return nodes;
}

function isAgentWorkItem(kind: chats.ChatItemKind): boolean {
  return kind === "Reasoning"
    || kind === "CommandExecution"
    || kind === "FileChange"
    || kind === "ToolCall"
    || kind === "WebSearch"
    || kind === "Subagent";
}

function workItemCounts(entries: ChatItemEntry[], summary: chats.ChatSummary): string[] {
  const counts = new Map<chats.ChatItemKind, number>();
  for (const entry of entries) counts.set(entry.kind, (counts.get(entry.kind) ?? 0) + 1);
  const kinds: chats.ChatItemKind[] = [
    "CommandExecution",
    "ToolCall",
    "FileChange",
    "WebSearch",
    "Subagent",
    "Reasoning",
  ];
  return kinds.flatMap((kind) => {
    const count = counts.get(kind);
    if (count === undefined) return [];

    const entriesOfKind = entries.filter((entry) => entry.kind === kind);
    const failed = entriesOfKind.filter(
      (entry) => displayItemState(entry, summary) === "Failed",
    ).length;
    if (kind === "Reasoning" || failed === 0) {
      return [`${count} ${workItemCountLabel(kind, count)}`];
    }

    const active = entriesOfKind.filter(
      (entry) => displayItemState(entry, summary) === "Started",
    ).length;
    const succeeded = count - failed - active;
    const statuses = [
      `${failed} failed`,
      succeeded ? `${succeeded} succeeded` : undefined,
      active ? `${active} active` : undefined,
    ].filter((status): status is string => status !== undefined);
    return [`${count} ${workItemCountLabel(kind, count)} (${statuses.join(", ")})`];
  });
}

function workItemCountLabel(kind: chats.ChatItemKind, count: number): string {
  switch (kind) {
    case "Reasoning":
      return "reasoning";
    case "CommandExecution":
      return count === 1 ? "command" : "commands";
    case "FileChange":
      return count === 1 ? "file change" : "file changes";
    case "ToolCall":
      return count === 1 ? "tool call" : "tool calls";
    case "WebSearch":
      return count === 1 ? "search" : "searches";
    case "Subagent":
      return count === 1 ? "delegation" : "delegations";
    default:
      return count === 1 ? "step" : "steps";
  }
}

function displayItemState(item: chats.ChatItem, summary: chats.ChatSummary): chats.ChatItemState {
  return item.state === "Started" && item.bindingId !== summary.binding?.bindingId
    ? "Failed"
    : item.state;
}

function isUnknownActivity(activity: chats.ChatActivity): boolean {
  return /^Unrecognized harness event:/i.test(activity.message);
}

function isEmptyReasoning(entry: chats.ChatTimelineEntry): boolean {
  return entry.entry === "Item"
    && entry.kind === "Reasoning"
    && entry.state === "Completed"
    && entry.content.every((part) => part.kind === "Text" && part.value.trim().length === 0);
}

function structuredMessageText(value: unknown): string | undefined {
  if (!value || typeof value !== "object") return undefined;
  const record = value as Record<string, unknown>;
  if (typeof record.text === "string") return record.text;
  if (!Array.isArray(record.content)) return undefined;
  const parts = record.content.flatMap((part) => {
    if (!part || typeof part !== "object") return [];
    const text = (part as Record<string, unknown>).text;
    return typeof text === "string" ? [text] : [];
  });
  return parts.length ? parts.join("\n") : undefined;
}

function structuredChatAttachment(value: unknown): PromptAttachmentDescriptor | undefined {
  if (!value || typeof value !== "object" || Array.isArray(value)) return undefined;
  const record = value as Record<string, unknown>;
  if (record.type !== "chatAttachment" || typeof record.name !== "string") return undefined;
  const mediaType = typeof record.mediaType === "string" ? record.mediaType : undefined;
  const size =
    typeof record.size === "number" && Number.isFinite(record.size) && record.size >= 0
      ? record.size
      : undefined;
  const attachmentId = typeof record.attachmentId === "string"
    ? record.attachmentId as chats.ChatAttachmentId
    : undefined;
  return { attachmentId, name: record.name, mediaType, size };
}
