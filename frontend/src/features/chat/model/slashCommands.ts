import type { config } from "../../../api/generated/index.ts";

export type WorkspaceSlashCommand = {
  name: string;
  text: string;
};

/** Converts configured slash commands into a stable presentation order. */
export function workspaceSlashCommands(
  commands: config.WorkspaceChatConfig["commands"],
): WorkspaceSlashCommand[] {
  return Object.entries(commands ?? {})
    .flatMap(([name, command]) => command
      ? [{
        name,
        text: command.text,
      }]
      : [])
    .toSorted((left, right) => left.name.localeCompare(right.name));
}

/** Returns the active command query when the draft contains only a slash expression. */
export function slashCommandQuery(text: string): string | undefined {
  return /^\/([a-z0-9-]*)$/i.exec(text)?.[1];
}
