import type { chats } from "../../../api/generated/index.ts";

export type ChatReplica = {
  summary: chats.ChatSummary;
  turnOrder: chats.ChatTurnId[];
  turnsById: ReadonlyMap<chats.ChatTurnId, chats.ChatTurn>;
  timelineOrder: string[];
  timelineById: ReadonlyMap<string, chats.ChatTimelineEntry>;
  attachmentOrder: chats.ChatAttachmentId[];
  attachmentsById: ReadonlyMap<chats.ChatAttachmentId, chats.ChatPromptAttachment>;
  queuedPrompts: chats.ChatQueuedPrompt[];
};

export function chatTurns(replica: ChatReplica): chats.ChatTurn[] {
  return orderedValues(replica.turnOrder, replica.turnsById, "turn");
}

export function chatTimeline(replica: ChatReplica): chats.ChatTimelineEntry[] {
  return orderedValues(replica.timelineOrder, replica.timelineById, "timeline entry");
}

export function timelineEntryKey(entry: chats.ChatTimelineEntry): string {
  switch (entry.entry) {
    case "Item":
      return `item-${entry.itemId}`;
    case "Request":
      return `request-${entry.requestId}`;
    case "Activity":
      return `activity-${entry.activityId}`;
  }
}

function orderedValues<K, V>(
  order: readonly K[],
  values: ReadonlyMap<K, V>,
  description: string,
): V[] {
  return order.map((key) => {
    const value = values.get(key);
    if (value === undefined) throw new Error(`Chat replica is missing ${description} ${String(key)}`);
    return value;
  });
}
