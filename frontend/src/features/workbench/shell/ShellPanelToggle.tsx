import { ChevronDown, ChevronLeft, ChevronRight, ChevronUp } from "lucide-react";
import { forwardRef, type ButtonHTMLAttributes } from "react";

import {
  formatKeyboardShortcut,
  KeyboardShortcut,
} from "../../../components/ui/KeyboardShortcut.tsx";

type ShellPanelToggleProps = Omit<ButtonHTMLAttributes<HTMLButtonElement>, "aria-label"> & {
  side: "left" | "right" | "bottom";
  expanded: boolean;
  label: string;
  shortcut: ReadonlyArray<string>;
  shortcutKeys: string;
};

export const ShellPanelToggle = forwardRef<HTMLButtonElement, ShellPanelToggleProps>(
  function ShellPanelToggle({
    side,
    expanded,
    label,
    shortcut,
    shortcutKeys,
    className,
    ...props
  }, ref) {
    const Icon = side === "bottom"
      ? expanded ? ChevronDown : ChevronUp
      : side === "left"
        ? expanded ? ChevronLeft : ChevronRight
        : expanded ? ChevronRight : ChevronLeft;
    const action = expanded ? "Collapse" : "Expand";

    return (
      <button
        {...props}
        className={`shell-panel-toggle ${expanded ? "shell-panel-toggle-expanded" : "shell-panel-toggle-collapsed"} ${className ?? ""}`}
        ref={ref}
        type="button"
        aria-label={`${action} ${label}`}
        aria-expanded={expanded}
        aria-keyshortcuts={shortcutKeys}
        title={`${action} ${label} (${formatKeyboardShortcut(shortcut)})`}
      >
        {expanded ? (
          <>
            <KeyboardShortcut keys={shortcut} />
            <Icon aria-hidden="true" size={13} />
          </>
        ) : (
          <span className={`shell-edge-handle shell-edge-handle-${side}`} aria-hidden="true">
            <Icon aria-hidden="true" size={13} />
          </span>
        )}
      </button>
    );
  },
);
