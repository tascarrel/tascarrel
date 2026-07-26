import { guestApi } from "../../api/client.ts";
import type { chats, store, workspaces } from "../../api/generated/index.ts";
import type { BackendStateDefinition } from "../../shared/state/BackendStateCache.ts";
import { useBackendState } from "../../shared/state/StateCacheProvider.tsx";
import { applyStoreEvent } from "../../shared/state/storeEvents.ts";
import {
  applyChatMutation,
  type ChatReplicaEvent,
  type ChatReplicaState,
  ChatSubscriptionProjector,
} from "./model/replicaSync.ts";

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
): BackendStateDefinition<ChatReplicaState, ChatReplicaEvent, store.Stamp> {
  return {
    key: `guest/${workspace}/chat/${chatId}`,
    retention: "lru",
    connect: (cursor, handlers) => {
      const projector = new ChatSubscriptionProjector();
      return guestApi(workspace).subscribe(
        "chats_Chat",
        () => ({ chatId, ...(cursor() ? { cursor: cursor() } : {}) }),
        {
          onEvent: (event) => {
            const projected = projector.project(event);
            if (projected) handlers.onEvent(projected);
          },
          onState: (state, attempt) => {
            if (state === "reconnecting") projector.reset();
            handlers.onConnection(state, attempt);
          },
          onError: handlers.onError,
        },
      );
    },
    applyEvent: (current, event) => {
      if (event.type === "Bootstrap") {
        return { value: event.replica, cursor: event.replica.stamp };
      }
      if (!current) throw new Error("Received a chat mutation before its bootstrap");
      return {
        value: applyChatMutation(current, event.mutation, event.stamp),
        cursor: event.stamp,
      };
    },
  };
}

function compareChatSummaries(left: chats.ChatSummary, right: chats.ChatSummary): number {
  return String(right.updatedAt).localeCompare(String(left.updatedAt))
    || String(left.chatId).localeCompare(String(right.chatId));
}
