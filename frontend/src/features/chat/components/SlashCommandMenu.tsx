import { type RefObject, useEffect, useRef } from "react";

import type { FuzzySearchItem } from "../../../components/ui/FuzzySearch.tsx";
import { KeyboardShortcut } from "../../../components/ui/KeyboardShortcut.tsx";
import { Popover } from "../../../components/ui/Popover.tsx";
import type { WorkspaceSlashCommand } from "../model/slashCommands.ts";

export function SlashCommandMenu({
  activeCommandName,
  anchor,
  id,
  items,
  onActiveIndexChange,
  onExpand,
}: {
  activeCommandName?: string;
  anchor: RefObject<HTMLElement | null>;
  id: string;
  items: readonly FuzzySearchItem<WorkspaceSlashCommand>[];
  onActiveIndexChange: (index: number) => void;
  onExpand: (command: WorkspaceSlashCommand) => void;
}) {
  const list = useRef<HTMLDivElement>(null);

  useEffect(() => {
    list.current
      ?.querySelector<HTMLElement>('[data-active="true"]')
      ?.scrollIntoView({ block: "nearest" });
  }, [activeCommandName]);

  return (
    <Popover.Root open>
      <Popover.Content
        anchor={anchor}
        className="!w-full !p-0"
        positionerClassName="w-[var(--anchor-width)] max-w-[calc(100vw-1.5rem)]"
        side="top"
        title="Workspace slash commands"
        titleClassName="sr-only"
      >
        <div className="bg-surface-raised/50">
          <div
            className="max-h-64 overflow-y-auto p-1.5"
            id={id}
            ref={list}
            role="listbox"
            aria-label="Workspace slash commands"
          >
            {items.length > 0 ? items.map((item, index) => {
              const active = item.value.name === activeCommandName;
              return (
                <button
                  className="grid w-full grid-cols-[auto_minmax(0,1fr)] items-center gap-4 rounded-lg px-3 py-2.5 text-left text-muted outline-none transition-colors hover:bg-surface-hover hover:text-foreground data-[active]:bg-surface-active data-[active]:text-foreground"
                  data-active={active || undefined}
                  id={slashCommandOptionId(id, item.value.name)}
                  key={item.id}
                  role="option"
                  tabIndex={-1}
                  type="button"
                  aria-selected={active}
                  onClick={() => onExpand(item.value)}
                  onMouseDown={(event) => event.preventDefault()}
                  onPointerMove={() => onActiveIndexChange(index)}
                >
                  <span className="min-w-0">
                    <strong className="block truncate font-mono text-xs font-medium text-inherit">
                      {item.label}
                    </strong>
                  </span>
                  <span className="min-w-0 truncate text-[11px] text-subtle">
                    {messagePreview(item.value.text)}
                  </span>
                </button>
              );
            }) : (
              <div className="px-3 py-5 text-center text-xs text-subtle" role="status">
                No matching workspace commands.
              </div>
            )}
          </div>
          <div className="flex flex-wrap items-center gap-x-4 gap-y-1 border-t border-ui-border px-3 py-2 text-[10px] text-subtle">
            <Shortcut keys={["Enter"]} label="Send" />
            <Shortcut keys={["Shift", "Enter"]} label="Edit" />
            <Shortcut keys={["Tab"]} label="Edit" />
            <Shortcut keys={["Esc"]} label="Close" />
          </div>
        </div>
      </Popover.Content>
    </Popover.Root>
  );
}

export function slashCommandOptionId(menuId: string, commandName: string): string {
  return `${menuId}-command-${commandName}`;
}

function Shortcut({ keys, label }: { keys: string[]; label: string }) {
  return (
    <span className="inline-flex items-center gap-1.5 whitespace-nowrap">
      <KeyboardShortcut keys={keys} />
      <span>{label}</span>
    </span>
  );
}

function messagePreview(message: string): string {
  return message.replace(/\s+/g, " ").trim();
}
