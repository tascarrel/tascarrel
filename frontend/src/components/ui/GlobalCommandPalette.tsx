import type { LucideIcon } from "lucide-react";
import {
  createContext,
  type ReactNode,
  useCallback,
  useContext,
  useEffect,
  useId,
  useMemo,
  useRef,
  useState,
} from "react";

import { CommandPalette, type CommandPaletteItem } from "./CommandPalette.tsx";
import { KeyboardShortcut } from "./KeyboardShortcut.tsx";

/** Declarative command contributed by one mounted application feature. */
export type GlobalCommandDefinition = {
  id: string;
  label: string;
  description?: string;
  group?: string;
  keywords?: readonly string[];
  icon?: LucideIcon;
  shortcut?: readonly string[];
  order?: number;
  available?: boolean;
  disabled?: boolean;
  perform: () => void | Promise<void>;
};

type CommandResolver = () => readonly GlobalCommandDefinition[];

type GlobalCommandRegistry = {
  register: (owner: string, resolve: CommandResolver) => () => void;
  commandsChanged: () => void;
};

type RegisteredCommand = {
  owner: string;
  definition: GlobalCommandDefinition;
};

type CommandReference = {
  owner: string;
  id: string;
};

const GlobalCommandContext = createContext<GlobalCommandRegistry | undefined>(undefined);

/** Hosts commands registered by mounted features and exposes them through one application palette. */
export function GlobalCommandPaletteProvider({ children }: { children: ReactNode }) {
  const registrations = useRef(new Map<string, CommandResolver>());
  const [, setRevision] = useState(0);
  const [open, setOpen] = useState(false);
  const commandsChanged = useCallback(() => setRevision((current) => current + 1), []);
  const register = useCallback((owner: string, resolve: CommandResolver) => {
    registrations.current.set(owner, resolve);
    commandsChanged();
    return () => {
      registrations.current.delete(owner);
      commandsChanged();
    };
  }, [commandsChanged]);
  const registry = useMemo(() => ({ register, commandsChanged }), [commandsChanged, register]);
  const commands = registeredCommands(registrations.current);
  const items = commands.map<CommandPaletteItem<CommandReference>>(({ owner, definition }, index) => {
    const Icon = definition.icon;
    return {
      id: `global-command-${index}`,
      value: { owner, id: definition.id },
      label: definition.label,
      description: [definition.group, definition.description].filter(Boolean).join(" · "),
      keywords: [...(definition.keywords ?? []), definition.group ?? ""],
      icon: Icon ? <Icon aria-hidden="true" size={15} /> : undefined,
      trailing: definition.shortcut
        ? <KeyboardShortcut keys={definition.shortcut} />
        : undefined,
      disabled: definition.disabled,
    };
  });

  useEffect(() => {
    const toggle = (event: KeyboardEvent) => {
      if (
        event.repeat
        || event.isComposing
        || event.altKey
        || event.shiftKey
        || (!event.metaKey && !event.ctrlKey)
        || event.key.toLowerCase() !== "k"
      ) return;
      event.preventDefault();
      event.stopPropagation();
      setOpen((current) => !current);
    };
    window.addEventListener("keydown", toggle, { capture: true });
    return () => window.removeEventListener("keydown", toggle, { capture: true });
  }, []);

  const runCommand = (reference: CommandReference) => {
    const command = registrations.current
      .get(reference.owner)
      ?.()
      .find((candidate) => candidate.id === reference.id);
    if (!command || command.disabled || command.available === false) return;
    try {
      const result = command.perform();
      if (result instanceof Promise) {
        void result.catch((cause) => console.error("Global command failed", cause));
      }
    } catch (cause) {
      console.error("Global command failed", cause);
    }
  };

  return (
    <GlobalCommandContext.Provider value={registry}>
      {children}
      <CommandPalette
        open={open}
        title="Command Palette"
        items={items}
        placeholder="Search commands…"
        emptyMessage="No matching commands."
        onOpenChange={setOpen}
        onSelect={runCommand}
      />
    </GlobalCommandContext.Provider>
  );
}

/** Registers commands for as long as the calling feature remains mounted. */
export function useGlobalCommands(commands: readonly GlobalCommandDefinition[]) {
  const registry = useContext(GlobalCommandContext);
  if (!registry) throw new Error("Global command palette provider is missing");
  const owner = useId();
  const commandsRef = useRef(commands);
  commandsRef.current = commands;
  const metadata = commandMetadata(commands);

  useEffect(
    () => registry.register(owner, () => commandsRef.current),
    [owner, registry],
  );

  useEffect(() => registry.commandsChanged(), [metadata, registry]);
}

function registeredCommands(
  registrations: ReadonlyMap<string, CommandResolver>,
): RegisteredCommand[] {
  return Array.from(registrations)
    .flatMap(([owner, resolve]) => resolve()
      .filter((command) => command.available !== false)
      .map((definition) => ({
        owner,
        definition,
      })))
    .toSorted((left, right) =>
      (left.definition.order ?? 0) - (right.definition.order ?? 0)
      || (left.definition.group ?? "").localeCompare(right.definition.group ?? "")
      || left.definition.label.localeCompare(right.definition.label)
    );
}

function commandMetadata(commands: readonly GlobalCommandDefinition[]): string {
  return JSON.stringify(commands.map((command) => ({
    id: command.id,
    label: command.label,
    description: command.description,
    group: command.group,
    keywords: command.keywords,
    shortcut: command.shortcut,
    order: command.order,
    available: command.available,
    disabled: command.disabled,
  })));
}
