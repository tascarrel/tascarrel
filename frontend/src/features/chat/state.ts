import { guestApi } from "../../api/client.ts";
import type { chats, store, workspaces } from "../../api/generated/index.ts";
import type { BackendStateDefinition } from "../../shared/state/BackendStateCache.ts";
import { useBackendState } from "../../shared/state/StateCacheProvider.tsx";
import { applyStoreEvent } from "../../shared/state/storeEvents.ts";

export function useChatList(workspace: workspaces.WorkspaceName) {
  return useBackendState(chatListDefinition(workspace));
}

export function useChatHarnesses(workspace: workspaces.WorkspaceName) {
  return useBackendState(chatHarnessesDefinition(workspace));
}

export function useChat(workspace: workspaces.WorkspaceName, chatId: chats.ChatId) {
  return useBackendState(chatDefinition(workspace, chatId));
}

function chatListDefinition(
  workspace: workspaces.WorkspaceName,
): BackendStateDefinition<chats.ChatList, chats.ChatListChangedEvent, store.Stamp> {
  return {
    key: `guest/${workspace}/chats`,
    connect: (cursor, handlers) => guestApi(workspace).subscribe(
      "chats_Changed",
      () => cursor() ? { cursor: cursor() } : {},
      {
        onEvent: handlers.onEvent,
        onState: handlers.onConnection,
        onError: handlers.onError,
      },
    ),
    applyEvent: (current, event) => applyStoreEvent(
      current,
      event.change,
      (list, mutation) => {
        if (mutation.type === "Remove") {
          return { chats: list.chats.filter((chat) => chat.chatId !== mutation.content) };
        }
        const index = list.chats.findIndex((chat) => chat.chatId === mutation.chatId);
        const next = index < 0
          ? [...list.chats, mutation]
          : list.chats.map((chat, candidateIndex) => candidateIndex === index ? mutation : chat);
        next.sort(compareChatSummaries);
        return { chats: next };
      },
    ),
  };
}

function chatHarnessesDefinition(
  workspace: workspaces.WorkspaceName,
): BackendStateDefinition<readonly chats.ChatHarness[], chats.ChatHarnessListEvent, never> {
  return {
    key: `guest/${workspace}/chat-harnesses`,
    connect: (_cursor, handlers) => guestApi(workspace).subscribe(
      "chats_HarnessList",
      {},
      {
        onEvent: handlers.onEvent,
        onState: handlers.onConnection,
        onError: handlers.onError,
      },
    ),
    applyEvent: (_current, event) => ({ value: [...event.harnesses] }),
  };
}

function chatDefinition(
  workspace: workspaces.WorkspaceName,
  chatId: chats.ChatId,
): BackendStateDefinition<chats.Chat, chats.ChatEvent, store.Stamp> {
  return {
    key: `guest/${workspace}/chat/${chatId}`,
    retention: "lru",
    connect: (cursor, handlers) => guestApi(workspace).subscribe(
      "chats_Chat",
      () => ({ chatId, ...(cursor() ? { cursor: cursor() } : {}) }),
      {
        onEvent: handlers.onEvent,
        onState: handlers.onConnection,
        onError: handlers.onError,
      },
    ),
    applyEvent: (current, event) => applyStoreEvent(current, event.change, applyChatMutation),
  };
}

function applyChatMutation(chat: chats.Chat, mutation: chats.ChatMutation): chats.Chat {
  switch (mutation.type) {
    case "UpdateSummary":
      return { ...chat, summary: mutation };
    case "UpsertTurn":
      return { ...chat, turns: upsert(chat.turns, mutation, (turn) => turn.turnId) };
    case "UpsertTimelineEntry":
      return {
        ...chat,
        timeline: upsert(chat.timeline, mutation.content, timelineEntryId),
      };
    case "AppendItemContent":
      return {
        ...chat,
        timeline: chat.timeline.map((entry) =>
          entry.entry === "Item" && entry.itemId === mutation.itemId
            ? appendItemText(entry, mutation.delta)
            : entry
        ),
      };
    case "CompleteTimelineItem":
      return {
        ...chat,
        timeline: chat.timeline.map((entry) =>
          entry.entry === "Item" && entry.itemId === mutation.itemId
            ? { ...entry, state: "Completed", completedAt: mutation.completedAt }
            : entry
        ),
      };
    case "ReplacePromptQueue":
      return { ...chat, queuedPrompts: [...mutation.prompts] };
    case "UpsertAttachment":
      return {
        ...chat,
        attachments: upsert(
          chat.attachments,
          mutation,
          (attachment) => attachment.attachmentId,
        ),
      };
  }
}

function appendItemText(
  item: Extract<chats.ChatTimelineEntry, { entry: "Item" }>,
  delta: string,
): chats.ChatTimelineEntry {
  let appended = false;
  const content = item.content.map((part) => {
    if (part.kind !== "Text" || appended) return part;
    appended = true;
    return { ...part, value: part.value + delta };
  });
  return { ...item, content };
}

function timelineEntryId(entry: chats.ChatTimelineEntry): string {
  switch (entry.entry) {
    case "Item":
      return String(entry.itemId);
    case "Request":
      return String(entry.requestId);
    case "Activity":
      return String(entry.activityId);
  }
}

function upsert<T>(values: readonly T[], value: T, id: (value: T) => unknown): T[] {
  const index = values.findIndex((candidate) => id(candidate) === id(value));
  if (index < 0) return [...values, value];
  return values.map((candidate, candidateIndex) => candidateIndex === index ? value : candidate);
}

function compareChatSummaries(left: chats.ChatSummary, right: chats.ChatSummary): number {
  return String(right.updatedAt).localeCompare(String(left.updatedAt))
    || String(left.chatId).localeCompare(String(right.chatId));
}
