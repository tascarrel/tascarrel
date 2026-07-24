import { Select } from "@base-ui/react/select";
import { Check, ChevronDown } from "lucide-react";

import { Badge, type BadgeTone } from "./Badge.tsx";

export interface SelectControlOption {
  label: string;
  value: string;
  badge?: {
    label: string;
    tone?: BadgeTone;
  };
}

export function SelectControl({
  label,
  value,
  options,
  disabled = false,
  hideLabel = false,
  variant = "muted",
  title,
  className = "",
  id,
  name,
  required = false,
  invalid = false,
  ariaDescribedBy,
  onBlur,
  onChange,
}: {
  label: string;
  value: string;
  options: SelectControlOption[];
  disabled?: boolean;
  hideLabel?: boolean;
  variant?: "default" | "muted" | "sidebar";
  title?: string;
  className?: string;
  id?: string;
  name?: string;
  required?: boolean;
  invalid?: boolean;
  ariaDescribedBy?: string;
  onBlur?: () => void;
  onChange: (value: string) => void;
}) {
  const selectedOption = options.find((option) => option.value === value);

  return (
    <Select.Root
      items={options}
      id={id}
      name={name}
      value={value}
      required={required}
      disabled={disabled}
      onValueChange={(nextValue) => {
        if (nextValue !== null) onChange(nextValue);
      }}
    >
      <div
        className={`group relative flex min-w-0 flex-col items-start ${hideLabel ? "" : "gap-1"} ${className}`}
        title={hideLabel ? undefined : title}
      >
        <Select.Label className={hideLabel ? "sr-only" : "text-[10px] text-subtle"}>{label}</Select.Label>
        {hideLabel ? (
          <span
            aria-hidden="true"
            className="pointer-events-none absolute bottom-[calc(100%+0.375rem)] left-1/2 z-20 -translate-x-1/2 whitespace-nowrap rounded-md border border-ui-border bg-surface-raised px-2 py-1 text-[10px] text-foreground opacity-0 shadow-lg shadow-black/30 transition-opacity group-focus-within:opacity-100 group-hover:opacity-100"
          >
            {label}
          </span>
        ) : null}
        <Select.Trigger
          aria-label={label}
          aria-describedby={ariaDescribedBy}
          aria-invalid={invalid || undefined}
          onBlur={onBlur}
          className={`flex h-9 w-full min-w-0 items-center justify-between gap-2 rounded-lg border px-2.5 text-xs outline-none transition focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-accent disabled:cursor-not-allowed disabled:opacity-50 aria-invalid:border-red-500/50 ${
            variant === "sidebar"
              ? "w-full border-transparent bg-transparent text-[13px] font-medium text-foreground hover:bg-surface-raised"
              : variant === "muted"
                ? "border-ui-border/70 bg-surface text-muted hover:border-ui-border-strong hover:bg-surface-raised"
                : "border-ui-border bg-surface-raised text-foreground hover:border-ui-border-strong"
          }`}
        >
          <Select.Value className="min-w-0 flex-1 truncate text-left" />
          {selectedOption?.badge ? <SelectOptionBadge badge={selectedOption.badge} /> : null}
          <Select.Icon className="shrink-0 text-subtle">
            <ChevronDown aria-hidden="true" className="size-3.5" />
          </Select.Icon>
        </Select.Trigger>
      </div>

      <Select.Portal>
        <Select.Positioner className="z-50 outline-none" sideOffset={6} alignItemWithTrigger={false}>
          <Select.Popup className="max-h-[min(20rem,var(--available-height))] min-w-[var(--anchor-width)] origin-[var(--transform-origin)] overflow-y-auto rounded-xl border border-ui-border-strong bg-surface-raised p-1 text-xs text-foreground shadow-2xl shadow-black/60 outline-none transition-[transform,opacity] data-[ending-style]:scale-95 data-[ending-style]:opacity-0 data-[starting-style]:scale-95 data-[starting-style]:opacity-0">
            <Select.List>
              {options.map((option) => (
                <Select.Item
                  className="grid cursor-default grid-cols-[1rem_minmax(0,1fr)_auto] items-center gap-1.5 rounded-lg px-2 py-1.5 outline-none data-[highlighted]:bg-ui-border data-[highlighted]:text-foreground data-[selected]:text-accent-text"
                  key={option.value}
                  value={option.value}
                >
                  <Select.ItemIndicator className="col-start-1 text-accent">
                    <Check aria-hidden="true" className="size-3.5" />
                  </Select.ItemIndicator>
                  <Select.ItemText className="col-start-2 truncate text-left">{option.label}</Select.ItemText>
                  {option.badge ? <SelectOptionBadge badge={option.badge} /> : null}
                </Select.Item>
              ))}
            </Select.List>
          </Select.Popup>
        </Select.Positioner>
      </Select.Portal>
    </Select.Root>
  );
}

function SelectOptionBadge({ badge }: { badge: NonNullable<SelectControlOption["badge"]> }) {
  return <Badge size="xs" tone={badge.tone ?? "muted"}>{badge.label}</Badge>;
}
