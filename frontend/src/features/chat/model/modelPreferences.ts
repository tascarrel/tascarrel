import type { chats, config } from "../../../api/generated/index.ts";
import { defaultModelSelection } from "./modelSelection.ts";

export function chatModelPreferences(
  settings: config.WorkspaceSettings | undefined,
  harness: chats.ChatHarnessKind,
): config.WorkspaceChatModelPreferences | undefined {
  if (harness === "Tasci") {
    const tasci = settings?.chat?.tasci;
    return tasci
      ? {
        defaultModel: tasci.defaultModel
          ? {
            model: tasci.defaultModel,
            options: [],
          }
          : undefined,
        modelOrder: tasci.modelOrder,
        hiddenModels: tasci.hiddenModels,
        favoriteModels: tasci.favoriteModels,
      }
      : undefined;
  }
  const harnesses = settings?.chat?.harnesses;
  return harness === "Codex" ? harnesses?.codex : harnesses?.claudeCode;
}

export function withChatModelPreferences(
  settings: config.WorkspaceSettings,
  harness: chats.ChatHarnessKind,
  preferences: config.WorkspaceChatModelPreferences,
): config.WorkspaceSettings {
  const chat = settings.chat ?? {};
  if (harness === "Tasci") {
    return {
      ...settings,
      chat: {
        ...chat,
        tasci: {
          ...(chat.tasci ?? {}),
          defaultModel: preferences.defaultModel?.model,
          modelOrder: preferences.modelOrder,
          hiddenModels: preferences.hiddenModels,
          favoriteModels: preferences.favoriteModels,
        },
      },
    };
  }

  const harnesses = chat.harnesses ?? {};
  return {
    ...settings,
    chat: {
      ...chat,
      harnesses: harness === "Codex"
        ? { ...harnesses, codex: preferences }
        : { ...harnesses, claudeCode: preferences },
    },
  };
}

export function visibleChatModels(
  harness: chats.ChatHarness | undefined,
  preferences: config.WorkspaceChatModelPreferences | undefined,
  selectedModel?: string,
): readonly chats.ChatModel[] {
  if (!harness) return [];
  const hidden = new Set(preferences?.hiddenModels ?? []);
  const favorites = new Set(preferences?.favoriteModels ?? []);
  const configuredOrder = new Map(
    (preferences?.modelOrder ?? []).map((model, index) => [model, index]),
  );
  const harnessOrder = new Map(harness.models.map((model, index) => [model.id, index]));

  return harness.models
    .filter((model) => !hidden.has(model.id) || model.id === selectedModel)
    .toSorted((left, right) => {
      const favoriteOrder = Number(favorites.has(right.id)) - Number(favorites.has(left.id));
      if (favoriteOrder !== 0) return favoriteOrder;
      const leftConfigured = configuredOrder.get(left.id) ?? Number.POSITIVE_INFINITY;
      const rightConfigured = configuredOrder.get(right.id) ?? Number.POSITIVE_INFINITY;
      if (leftConfigured !== rightConfigured) return leftConfigured - rightConfigured;
      return (harnessOrder.get(left.id) ?? 0) - (harnessOrder.get(right.id) ?? 0);
    });
}

export function preferredDefaultModelSelection(
  harness: chats.ChatHarness | undefined,
  preferences: config.WorkspaceChatModelPreferences | undefined,
): chats.ChatModelSelection | undefined {
  if (!harness?.models.length) return preferences?.defaultModel;
  if (harness.models.some((model) => model.id === preferences?.defaultModel?.model)) {
    return preferences?.defaultModel;
  }
  const hidden = new Set(preferences?.hiddenModels ?? []);
  return defaultModelSelection(
    harness.models.find((model) => !hidden.has(model.id)) ?? harness.models[0],
  );
}

export function isFavoriteChatModel(
  preferences: config.WorkspaceChatModelPreferences | undefined,
  model: string,
): boolean {
  return preferences?.favoriteModels?.includes(model) ?? false;
}
