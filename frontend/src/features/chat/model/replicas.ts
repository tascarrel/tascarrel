import type { chats } from "../../../api/generated/index.ts";

export type ChatReplica = chats.Chat;

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
