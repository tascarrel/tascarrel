import type { chats, config } from "../../../api/generated/index.ts";
import { SelectControl } from "../../../components/ui/SelectControl.tsx";
import {
  isFavoriteChatModel,
  visibleChatModels,
} from "../model/modelPreferences.ts";
import {
  defaultModelSelection,
  selectedOption,
  updateModelOption,
} from "../model/modelSelection.ts";

export function ModelControls({
  harness,
  preferences,
  selection,
  disabled = false,
  hideLabels = false,
  onChange,
}: {
  harness?: chats.ChatHarness;
  preferences?: config.WorkspaceChatModelPreferences;
  selection?: chats.ChatModelSelection;
  disabled?: boolean;
  hideLabels?: boolean;
  onChange: (selection: chats.ChatModelSelection | undefined) => void;
}) {
  const models = visibleChatModels(harness, preferences, selection?.model);
  const model =
    harness?.models.find((candidate) => candidate.id === selection?.model) ?? models[0];
  const effectiveSelection = selection ?? defaultModelSelection(model);

  return (
    <div className="flex flex-wrap items-end gap-x-2 gap-y-2">
      <SelectControl
        className="text-xs text-muted"
        disabled={disabled || !models.length}
        hideLabel={hideLabels}
        label="Model"
        options={
          models.map((candidate) => ({
            label: `${isFavoriteChatModel(preferences, candidate.id) ? "★ " : ""}${candidate.shortName ?? candidate.displayName}`,
            value: candidate.id,
          }))
        }
        value={model?.id ?? ""}
        onChange={(modelId) => {
          const next = harness?.models.find((candidate) => candidate.id === modelId);
          onChange(defaultModelSelection(next));
        }}
      />

      {model?.options.map((descriptor) => {
        if (!effectiveSelection) return null;
        if (descriptor.kind === "Select") {
          const selected = selectedOption(effectiveSelection, descriptor.id);
          const defaultChoice = descriptor.choices.find((choice) => choice.isDefault);
          const value = selected?.kind === "String" ? selected.content : (defaultChoice?.id ?? "");
          return (
            <SelectControl
              className="text-xs text-muted"
              disabled={disabled}
              hideLabel={hideLabels}
              key={descriptor.id}
              label={descriptor.label}
              options={[
                ...(!value
                  ? [{ label: `Default ${descriptor.label.toLowerCase()}`, value: "" }]
                  : []),
                ...descriptor.choices.map((choice) => ({
                  label: choice.label,
                  value: choice.id,
                })),
              ]}
              title={descriptor.description}
              value={value}
              onChange={(nextValue) =>
                onChange(
                  updateModelOption(effectiveSelection, descriptor.id, {
                    kind: "String",
                    content: nextValue,
                  }),
                )
              }
            />
          );
        }

        const selected = selectedOption(effectiveSelection, descriptor.id);
        const checked = selected?.kind === "Boolean" ? selected.content : false;
        return (
          <label
            className={`flex cursor-pointer flex-col items-start text-xs text-muted ${hideLabels ? "" : "gap-1"}`}
            key={descriptor.id}
            title={descriptor.description}
          >
            <span className={hideLabels ? "sr-only" : "text-[10px] text-subtle"}>{descriptor.label}</span>
            <span className="flex h-9 items-center gap-2 rounded-lg border border-ui-border/70 bg-surface px-2.5">
              <input
                className="size-3.5 accent-brand"
                type="checkbox"
                checked={checked}
                disabled={disabled}
                onChange={(event) =>
                  onChange(
                    updateModelOption(effectiveSelection, descriptor.id, {
                      kind: "Boolean",
                      content: event.target.checked,
                    }),
                  )
                }
              />
              {checked ? "Enabled" : "Disabled"}
            </span>
          </label>
        );
      })}
    </div>
  );
}
