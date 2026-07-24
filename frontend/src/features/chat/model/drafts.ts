import type { chats } from "../../../api/generated/index.ts";

export type ChatComposerDraft = {
  text: string;
  attachments: chats.ChatPromptAttachment[];
  mode: chats.ChatPromptMode;
  model?: chats.ChatModelSelection;
};

export type ChatCreatorDraft = {
  title: string;
  harnessKey?: string;
};

export function loadChatComposerDraft(draftId: string): ChatComposerDraft | undefined {
  try {
    const stored = window.localStorage.getItem(storageKey(draftId));
    if (!stored) return undefined;
    const value = JSON.parse(stored) as Partial<StoredChatComposerDraft>;
    if (value.version !== 1 || typeof value.text !== "string" || !Array.isArray(value.attachments)) {
      return undefined;
    }
    return {
      text: value.text,
      attachments: value.attachments,
      mode: value.mode ?? "WhenIdle",
      ...(value.model ? { model: value.model } : {}),
    };
  } catch {
    return undefined;
  }
}

export function storeChatComposerDraft(draftId: string, draft: ChatComposerDraft): void {
  try {
    window.localStorage.setItem(
      storageKey(draftId),
      JSON.stringify({ version: 1, ...draft } satisfies StoredChatComposerDraft),
    );
  } catch {
    // Draft persistence is best-effort when browser storage is unavailable.
  }
}

export function removeChatComposerDraft(draftId: string): void {
  try {
    window.localStorage.removeItem(storageKey(draftId));
  } catch {
    // Draft persistence is best-effort when browser storage is unavailable.
  }
}

export function loadChatCreatorDraft(draftId: string): ChatCreatorDraft | undefined {
  try {
    const stored = window.localStorage.getItem(creatorStorageKey(draftId));
    if (!stored) return undefined;
    const value = JSON.parse(stored) as Partial<StoredChatCreatorDraft>;
    if (value.version !== 1 || typeof value.title !== "string") return undefined;
    return {
      title: value.title,
      ...(typeof value.harnessKey === "string" ? { harnessKey: value.harnessKey } : {}),
    };
  } catch {
    return undefined;
  }
}

export function storeChatCreatorDraft(draftId: string, draft: ChatCreatorDraft): void {
  try {
    window.localStorage.setItem(
      creatorStorageKey(draftId),
      JSON.stringify({ version: 1, ...draft } satisfies StoredChatCreatorDraft),
    );
  } catch {
    // Draft persistence is best-effort when browser storage is unavailable.
  }
}

export function removeChatCreatorDraft(draftId: string): void {
  try {
    window.localStorage.removeItem(creatorStorageKey(draftId));
  } catch {
    // Draft persistence is best-effort when browser storage is unavailable.
  }
}

type StoredChatComposerDraft = ChatComposerDraft & {
  version: 1;
};

type StoredChatCreatorDraft = ChatCreatorDraft & {
  version: 1;
};

function storageKey(draftId: string): string {
  return `tascarrel.chat.draft.v1.${draftId}`;
}

function creatorStorageKey(draftId: string): string {
  return `tascarrel.chat.creator-draft.v1.${draftId}`;
}
