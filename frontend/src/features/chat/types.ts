import type { chats, config } from "../../api/generated/index.ts";

import type { ChatReplica } from "./model/replicas.ts";

export type ChatConnectionStatus = "connecting" | "live" | "reconnecting" | "stopped";

export type PromptSubmission = {
  prompt: chats.ChatPrompt;
  mode: chats.ChatPromptMode;
};

export type AttachmentUploader = (file: File) => Promise<chats.ChatPromptAttachment>;

export type AttachmentUrlResolver = (attachmentId: chats.ChatAttachmentId) => string;

export type StartChatSubmission = {
  harness: chats.ChatHarnessKind;
  title?: string;
  model?: chats.ChatModelSelection;
  prompt: chats.ChatPrompt;
};

export type ChatScreenActions = {
  sendPrompt: (submission: PromptSubmission) => Promise<chats.SendChatPromptOutput>;
  interrupt: () => Promise<void>;
  compactContext: () => Promise<void>;
  attach: () => Promise<void>;
  detach: () => Promise<void>;
  archive: () => Promise<void>;
  flushPromptQueue: () => Promise<void>;
  removeQueuedPrompt: (queuedPromptId: chats.ChatQueuedPromptId) => Promise<void>;
  resolveRequest: (
    requestId: chats.ChatRequestId,
    answers: chats.ChatQuestionAnswer[],
  ) => Promise<void>;
};

export type ChatScreenProps = {
  summary: chats.ChatSummary;
  replica?: ChatReplica;
  harness?: chats.ChatHarness;
  modelPreferences?: config.WorkspaceChatModelPreferences;
  slashCommands?: config.WorkspaceChatConfig["commands"];
  status: ChatConnectionStatus;
  actions: ChatScreenActions;
  attachmentUploader?: AttachmentUploader;
  attachmentUrl?: AttachmentUrlResolver;
  onError: (cause: unknown) => void;
  showUnknownItems?: boolean;
};
