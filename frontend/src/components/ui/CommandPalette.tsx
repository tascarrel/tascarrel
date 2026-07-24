import { Dialog } from "@base-ui/react/dialog";
import { useEffect, useState } from "react";

import { FuzzySearch, type FuzzySearchItem } from "./FuzzySearch.tsx";

export type CommandPaletteItem<Value> = FuzzySearchItem<Value>;

/** Presents a searchable modal picker for a caller-defined set of commands or values. */
export function CommandPalette<Value>({
  open,
  title,
  description,
  items,
  placeholder = "Search…",
  emptyMessage = "No matching commands.",
  onOpenChange,
  onSelect,
}: {
  open: boolean;
  title: string;
  description?: string;
  items: readonly CommandPaletteItem<Value>[];
  placeholder?: string;
  emptyMessage?: string;
  onOpenChange: (open: boolean) => void;
  onSelect: (value: Value) => void;
}) {
  const [query, setQuery] = useState("");

  useEffect(() => {
    if (open) setQuery("");
  }, [open]);

  const select = (value: Value) => {
    onOpenChange(false);
    onSelect(value);
  };

  return (
    <Dialog.Root open={open} onOpenChange={onOpenChange}>
      <Dialog.Portal>
        <Dialog.Backdrop className="fixed inset-0 z-[80] bg-black/65 backdrop-blur-[2px] transition-opacity data-[ending-style]:opacity-0 data-[starting-style]:opacity-0" />
        <Dialog.Viewport className="fixed inset-0 z-[80] flex items-start justify-center overflow-y-auto px-4 pt-[14vh]">
          <Dialog.Popup className="w-full max-w-xl overflow-hidden rounded-xl border border-ui-border-strong bg-surface-raised text-foreground shadow-2xl shadow-black/70 outline-none transition-[transform,opacity] data-[ending-style]:scale-[0.98] data-[ending-style]:opacity-0 data-[starting-style]:scale-[0.98] data-[starting-style]:opacity-0">
            <div className="border-b border-divider px-4 py-3">
              <Dialog.Title className="text-sm font-semibold">{title}</Dialog.Title>
              {description ? (
                <Dialog.Description className="mt-1 text-[11px] leading-4 text-subtle">
                  {description}
                </Dialog.Description>
              ) : null}
            </div>
            <FuzzySearch
              items={items}
              query={query}
              searchLabel={`Search ${title}`}
              placeholder={placeholder}
              emptyMessage={emptyMessage}
              onQueryChange={setQuery}
              onSelect={select}
            />
          </Dialog.Popup>
        </Dialog.Viewport>
      </Dialog.Portal>
    </Dialog.Root>
  );
}
