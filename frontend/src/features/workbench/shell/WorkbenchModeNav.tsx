import { Toggle, ToggleGroup } from "@base-ui/react";
import { Bot, Box, Code2, Files, GitPullRequest, type LucideIcon } from "lucide-react";

import type { WorkspaceView } from "../../../app/router.tsx";

export type WorkbenchMode = WorkspaceView;

const MODES = [
  { value: "agent", label: "Agent", icon: Bot },
  { value: "code", label: "Code", icon: Code2 },
  { value: "changes", label: "Changes", icon: GitPullRequest },
  { value: "files", label: "Files", icon: Files },
  { value: "pod", label: "Pod", icon: Box },
] satisfies Array<ShellModeOption<WorkbenchMode>>;

export type ShellModeOption<Value extends string> = {
  value: Value;
  label: string;
  icon: LucideIcon;
};

export function ShellModeNav<Value extends string>({
  value,
  options,
  label,
  onValueChange,
}: {
  value: Value;
  options: Array<ShellModeOption<Value>> | ReadonlyArray<ShellModeOption<Value>>;
  label: string;
  onValueChange?: (value: Value) => void;
}) {
  return (
    <ToggleGroup
      className="shell-mode-nav"
      value={[value]}
      onValueChange={(next) => {
        const selected = next.at(-1) as Value | undefined;
        if (selected) onValueChange?.(selected);
      }}
      aria-label={label}
    >
      {options.map(({ value: mode, label: modeLabel, icon: Icon }) => (
        <Toggle
          className="shell-mode-button"
          value={mode}
          aria-label={modeLabel}
          title={modeLabel}
          key={mode}
        >
          <Icon aria-hidden="true" size={14} />
        </Toggle>
      ))}
    </ToggleGroup>
  );
}

export function WorkbenchModeNav({
  value,
  onValueChange,
}: {
  value: WorkbenchMode;
  onValueChange: (value: WorkbenchMode) => void;
}) {
  return (
    <ShellModeNav
      value={value}
      options={MODES}
      label="Workbench view"
      onValueChange={onValueChange}
    />
  );
}
