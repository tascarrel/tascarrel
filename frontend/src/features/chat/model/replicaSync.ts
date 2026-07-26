import type { chats, store } from "../../../api/generated/index.ts";

import type { ChatReplica } from "./replicas.ts";
import { timelineEntryKey } from "./replicas.ts";

export type ChatReplicaState = ChatReplica & {
  stamp: store.Stamp;
};

export type ChatReplicaEvent =
  | { type: "Bootstrap"; replica: ChatReplicaState }
  | { type: "Mutation"; stamp: store.Stamp; mutation: chats.ChatMutation };

type ChatBootstrap = {
  stamp: store.Stamp;
  summary: chats.ChatSummary;
  expectedTurns: number;
  expectedTimeline: number;
  expectedAttachments: number;
  expectedQueuedPrompts: number;
  turns: chats.ChatTurn[];
  timeline: chats.ChatTimelineEntry[];
  attachments: chats.ChatPromptAttachment[];
  queuedPrompts: chats.ChatQueuedPrompt[];
};

export class ChatSubscriptionProjector {
  private bootstrap: ChatBootstrap | undefined;

  public reset(): void {
    this.bootstrap = undefined;
  }

  public project(event: chats.ChatEvent): ChatReplicaEvent | undefined {
    const change = event.change;
    switch (change.type) {
      case "BootstrapStarted":
        this.bootstrap = {
          stamp: change.stamp,
          summary: change.summary,
          expectedTurns: change.turnCount,
          expectedTimeline: change.timelineCount,
          expectedAttachments: change.attachmentCount,
          expectedQueuedPrompts: change.queuedPromptCount,
          turns: [],
          timeline: [],
          attachments: [],
          queuedPrompts: [],
        };
        return undefined;
      case "BootstrapTurns": {
        const bootstrap = this.requireBootstrap(change.stamp);
        appendBootstrapRange(
          bootstrap.turns,
          change.offset,
          change.turns,
          bootstrap.expectedTurns,
          "turn",
        );
        return undefined;
      }
      case "BootstrapTimeline": {
        const bootstrap = this.requireBootstrap(change.stamp);
        appendBootstrapRange(
          bootstrap.timeline,
          change.offset,
          change.entries,
          bootstrap.expectedTimeline,
          "timeline",
        );
        return undefined;
      }
      case "BootstrapAttachments": {
        const bootstrap = this.requireBootstrap(change.stamp);
        appendBootstrapRange(
          bootstrap.attachments,
          change.offset,
          change.attachments,
          bootstrap.expectedAttachments,
          "attachment",
        );
        return undefined;
      }
      case "BootstrapPromptQueue": {
        const bootstrap = this.requireBootstrap(change.stamp);
        appendBootstrapRange(
          bootstrap.queuedPrompts,
          change.offset,
          change.prompts,
          bootstrap.expectedQueuedPrompts,
          "queued prompt",
        );
        return undefined;
      }
      case "BootstrapCompleted": {
        const bootstrap = this.requireBootstrap(change.stamp);
        const replica = completeBootstrap(bootstrap);
        this.bootstrap = undefined;
        return { type: "Bootstrap", replica };
      }
      case "Mutation":
        if (this.bootstrap) {
          throw new Error("Received a chat mutation before bootstrap completion");
        }
        return {
          type: "Mutation",
          stamp: change.stamp,
          mutation: change.mutation,
        };
    }
  }

  private requireBootstrap(stamp: store.Stamp): ChatBootstrap {
    if (!this.bootstrap || !sameStamp(this.bootstrap.stamp, stamp)) {
      throw new Error("Received a chat bootstrap range outside its snapshot");
    }
    return this.bootstrap;
  }
}

export function applyChatMutation(
  chat: ChatReplicaState,
  mutation: chats.ChatMutation,
  stamp: store.Stamp,
): ChatReplicaState {
  requireNextStamp(chat.stamp, stamp);
  switch (mutation.type) {
    case "UpdateSummary":
      if (mutation.chatId !== chat.summary.chatId) {
        throw new Error("Chat summary mutation changed the chat identifier");
      }
      return { ...chat, stamp, summary: mutation };
    case "UpsertTurn": {
      const exists = chat.turnsById.has(mutation.turnId);
      const turnsById = new Map(chat.turnsById);
      turnsById.set(mutation.turnId, mutation);
      return {
        ...chat,
        stamp,
        turnOrder: exists ? chat.turnOrder : [...chat.turnOrder, mutation.turnId],
        turnsById,
      };
    }
    case "UpsertTimelineEntry": {
      const key = timelineEntryKey(mutation.content);
      const exists = chat.timelineById.has(key);
      const timelineById = new Map(chat.timelineById);
      timelineById.set(key, mutation.content);
      return {
        ...chat,
        stamp,
        timelineOrder: exists ? chat.timelineOrder : [...chat.timelineOrder, key],
        timelineById,
      };
    }
    case "AppendItemContent": {
      const updated = updateTimelineItem(chat, mutation.itemId, (item) =>
        appendItemText(item, mutation.delta)
      );
      return { ...updated, stamp };
    }
    case "CompleteTimelineItem": {
      const updated = updateTimelineItem(chat, mutation.itemId, (item) => ({
        ...item,
        state: "Completed",
        completedAt: mutation.completedAt,
      }));
      return { ...updated, stamp };
    }
    case "ReplacePromptQueue":
      return { ...chat, stamp, queuedPrompts: [...mutation.prompts] };
    case "UpsertAttachment": {
      const exists = chat.attachmentsById.has(mutation.attachmentId);
      const attachmentsById = new Map(chat.attachmentsById);
      attachmentsById.set(mutation.attachmentId, mutation);
      const attachmentOrder = exists
        ? chat.attachmentOrder
        : [...chat.attachmentOrder, mutation.attachmentId].sort(compareIds);
      return {
        ...chat,
        stamp,
        attachmentOrder,
        attachmentsById,
      };
    }
  }
}

function appendBootstrapRange<T>(
  target: T[],
  offset: number,
  values: readonly T[],
  expectedLength: number,
  description: string,
): void {
  if (offset !== target.length) {
    throw new Error(
      `Received ${description} bootstrap range at ${offset}, expected ${target.length}`,
    );
  }
  if (target.length + values.length > expectedLength) {
    throw new Error(`Received too many ${description} bootstrap values`);
  }
  for (const value of values) target.push(value);
}

function completeBootstrap(bootstrap: ChatBootstrap): ChatReplicaState {
  requireCompleteCollection(bootstrap.turns, bootstrap.expectedTurns, "turn");
  requireCompleteCollection(bootstrap.timeline, bootstrap.expectedTimeline, "timeline");
  requireCompleteCollection(
    bootstrap.attachments,
    bootstrap.expectedAttachments,
    "attachment",
  );
  requireCompleteCollection(
    bootstrap.queuedPrompts,
    bootstrap.expectedQueuedPrompts,
    "queued prompt",
  );
  const turnsById = uniqueValues(bootstrap.turns, (turn) => turn.turnId, "turn");
  const timelineById = uniqueValues(bootstrap.timeline, timelineEntryKey, "timeline entry");
  const attachmentsById = uniqueValues(
    bootstrap.attachments,
    (attachment) => attachment.attachmentId,
    "attachment",
  );
  return {
    stamp: bootstrap.stamp,
    summary: bootstrap.summary,
    turnOrder: bootstrap.turns.map((turn) => turn.turnId),
    turnsById,
    timelineOrder: bootstrap.timeline.map(timelineEntryKey),
    timelineById,
    attachmentOrder: bootstrap.attachments.map((attachment) => attachment.attachmentId),
    attachmentsById,
    queuedPrompts: [...bootstrap.queuedPrompts],
  };
}

function requireCompleteCollection(
  values: readonly unknown[],
  expectedLength: number,
  description: string,
): void {
  if (values.length !== expectedLength) {
    throw new Error(
      `Chat bootstrap completed with ${values.length} ${description} values, expected ${expectedLength}`,
    );
  }
}

function uniqueValues<K, V>(
  values: readonly V[],
  key: (value: V) => K,
  description: string,
): ReadonlyMap<K, V> {
  const indexed = new Map<K, V>();
  for (const value of values) {
    const valueKey = key(value);
    if (indexed.has(valueKey)) {
      throw new Error(`Chat bootstrap contains duplicate ${description} ${String(valueKey)}`);
    }
    indexed.set(valueKey, value);
  }
  return indexed;
}

function appendItemText(
  item: Extract<chats.ChatTimelineEntry, { entry: "Item" }>,
  delta: string,
): chats.ChatTimelineEntry {
  const textIndices = item.content.flatMap((part, index) => part.kind === "Text" ? [index] : []);
  if (textIndices.length !== 1) {
    throw new Error("Streaming chat item does not contain exactly one text value");
  }
  const content = [...item.content];
  const textIndex = textIndices[0];
  const text = content[textIndex];
  if (text.kind !== "Text") throw new Error("Streaming chat item text target changed kind");
  content[textIndex] = { ...text, value: text.value + delta };
  return { ...item, content };
}

function updateTimelineItem(
  chat: ChatReplicaState,
  itemId: chats.ChatItemId,
  update: (
    item: Extract<chats.ChatTimelineEntry, { entry: "Item" }>,
  ) => chats.ChatTimelineEntry,
): ChatReplicaState {
  const key = `item-${itemId}`;
  const entry = chat.timelineById.get(key);
  if (!entry || entry.entry !== "Item") {
    throw new Error(`Cannot update unknown chat item ${itemId}`);
  }
  const timelineById = new Map(chat.timelineById);
  timelineById.set(key, update(entry));
  return { ...chat, timelineById };
}

function requireNextStamp(current: store.Stamp, next: store.Stamp): void {
  if (
    current.generation !== next.generation
    || BigInt(next.version) !== BigInt(current.version) + 1n
  ) {
    throw new Error("Received a chat mutation outside the replica's store sequence");
  }
}

function sameStamp(left: store.Stamp, right: store.Stamp): boolean {
  return left.generation === right.generation
    && BigInt(left.version) === BigInt(right.version);
}

function compareIds(left: unknown, right: unknown): number {
  const leftId = String(left);
  const rightId = String(right);
  return leftId < rightId ? -1 : leftId > rightId ? 1 : 0;
}
