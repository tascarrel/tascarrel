import type { chats } from "../../../api/generated/index.ts";

export function defaultModelSelection(model: chats.ChatModel | undefined): chats.ChatModelSelection | undefined {
  if (!model) return undefined;
  return {
    model: model.id,
    options: model.options.flatMap((descriptor) => {
      if (descriptor.kind === "Select") {
        const choice = descriptor.choices.find((candidate) => candidate.isDefault);
        return choice
          ? [{ id: descriptor.id, value: { kind: "String", content: choice.id } }]
          : [];
      }
      return [];
    }),
  };
}

export function reconcileModelSelection(
  harness: chats.ChatHarness | undefined,
  selection: chats.ChatModelSelection | undefined,
  fallback?: chats.ChatModelSelection,
): chats.ChatModelSelection | undefined {
  if (!harness?.models.length) return selection;
  const requested = harness.models.some((candidate) => candidate.id === selection?.model)
    ? selection
    : harness.models.some((candidate) => candidate.id === fallback?.model)
      ? fallback
      : undefined;
  const model = harness.models.find((candidate) => candidate.id === requested?.model) ?? harness.models[0];
  if (!requested || requested.model !== model.id) return defaultModelSelection(model);

  const options = model.options.flatMap((descriptor) => {
    const existing = requested.options.find((candidate) => candidate.id === descriptor.id);
    if (descriptor.kind === "Select") {
      if (
        existing?.value.kind === "String"
        && descriptor.choices.some((choice) => choice.id === existing.value.content)
      ) return [existing];
      const choice = descriptor.choices.find((candidate) => candidate.isDefault);
      return choice
        ? [{ id: descriptor.id, value: { kind: "String" as const, content: choice.id } }]
        : [];
    }
    return existing?.value.kind === "Boolean" ? [existing] : [];
  });
  return { model: model.id, options };
}

export function updateModelOption(
  selection: chats.ChatModelSelection,
  id: string,
  value: chats.ChatModelOptionValue,
): chats.ChatModelSelection {
  const options = selection.options.filter((candidate) => candidate.id !== id);
  options.push({ id, value });
  return { ...selection, options };
}

export function selectedOption(
  selection: chats.ChatModelSelection | undefined,
  id: string,
): chats.ChatModelOptionValue | undefined {
  return selection?.options.find((candidate) => candidate.id === id)?.value;
}
