import { Toggle, ToggleGroup } from "@base-ui/react";

export type SegmentedControlOption<Value extends string> = {
  value: Value;
  label: string;
};

export function SegmentedControl<Value extends string>({
  value,
  options,
  label,
  onValueChange,
}: {
  value: Value;
  options: ReadonlyArray<SegmentedControlOption<Value>>;
  label: string;
  onValueChange: (value: Value) => void;
}) {
  return (
    <ToggleGroup
      aria-label={label}
      className="inline-flex h-8 items-center rounded-lg border border-ui-border/70 bg-surface p-0.5"
      value={[value]}
      onValueChange={(next) => {
        const selected = next.at(-1) as Value | undefined;
        if (selected) onValueChange(selected);
      }}
    >
      {options.map((option) => (
        <Toggle
          className="h-6 rounded-md px-2 text-[11px] font-medium text-subtle outline-none transition hover:text-foreground focus-visible:outline-2 focus-visible:outline-accent data-[pressed]:bg-surface-raised data-[pressed]:text-foreground"
          key={option.value}
          value={option.value}
        >
          {option.label}
        </Toggle>
      ))}
    </ToggleGroup>
  );
}
