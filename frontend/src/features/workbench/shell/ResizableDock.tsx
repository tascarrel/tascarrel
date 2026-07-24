import { Collapsible } from "@base-ui/react/collapsible";
import {
  GripHorizontal,
  GripVertical,
} from "lucide-react";
import type {
  CSSProperties,
  KeyboardEvent as ReactKeyboardEvent,
  PointerEvent as ReactPointerEvent,
  ReactNode,
} from "react";

import { ShellPanelToggle } from "./ShellPanelToggle.tsx";

type ResizableDockProps = {
  side: "bottom" | "right";
  label: string;
  open: boolean;
  size: number;
  minSize: number;
  maxSize: number;
  defaultSize: number;
  shortcut: ReadonlyArray<string>;
  shortcutKeys: string;
  onOpenChange: (open: boolean) => void;
  onSizeChange: (size: number) => void;
  header: ReactNode;
  children: ReactNode;
};

export function ResizableDock({
  side,
  label,
  open,
  size,
  minSize,
  maxSize,
  defaultSize,
  shortcut,
  shortcutKeys,
  onOpenChange,
  onSizeChange,
  header,
  children,
}: ResizableDockProps) {
  const currentSize = open ? size : 0;
  const style: CSSProperties = side === "bottom"
    ? { height: currentSize }
    : { width: currentSize };

  const resize = (event: ReactPointerEvent<HTMLDivElement>) => {
    if (!open || event.button !== 0) return;
    event.preventDefault();
    const handle = event.currentTarget;
    const origin = side === "bottom" ? event.clientY : event.clientX;
    const startSize = size;
    const parent = handle.parentElement?.parentElement;
    const available = parent
      ? side === "bottom" ? parent.clientHeight : parent.clientWidth
      : maxSize;
    const maximum = Math.max(
      minSize,
      Math.min(maxSize, available - (side === "bottom" ? 180 : 320)),
    );
    const previousCursor = document.body.style.cursor;
    const previousSelection = document.body.style.userSelect;
    document.body.style.cursor = side === "bottom" ? "ns-resize" : "ew-resize";
    document.body.style.userSelect = "none";
    handle.setPointerCapture(event.pointerId);

    const move = (moveEvent: PointerEvent) => {
      const position = side === "bottom" ? moveEvent.clientY : moveEvent.clientX;
      onSizeChange(clamp(startSize - (position - origin), minSize, maximum));
    };
    const finish = () => {
      document.body.style.cursor = previousCursor;
      document.body.style.userSelect = previousSelection;
      handle.removeEventListener("pointermove", move);
      handle.removeEventListener("pointerup", finish);
      handle.removeEventListener("pointercancel", finish);
    };
    handle.addEventListener("pointermove", move);
    handle.addEventListener("pointerup", finish);
    handle.addEventListener("pointercancel", finish);
  };

  const resizeWithKeyboard = (event: ReactKeyboardEvent<HTMLDivElement>) => {
    const direction = side === "bottom"
      ? event.key === "ArrowUp" ? 1 : event.key === "ArrowDown" ? -1 : 0
      : event.key === "ArrowLeft" ? 1 : event.key === "ArrowRight" ? -1 : 0;
    if (direction === 0 && event.key !== "Home" && event.key !== "End") return;
    event.preventDefault();
    if (event.key === "Home") onSizeChange(minSize);
    else if (event.key === "End") onSizeChange(maxSize);
    else onSizeChange(clamp(size + direction * 18, minSize, maxSize));
  };

  return (
    <Collapsible.Root
      className={`resizable-dock resizable-dock-${side}`}
      data-open={open || undefined}
      open={open}
      onOpenChange={onOpenChange}
      style={style}
    >
      {open ? (
        <div
          className="resizable-dock-handle"
          role="separator"
          aria-label={`Resize ${label}`}
          aria-orientation={side === "bottom" ? "horizontal" : "vertical"}
          aria-valuemin={minSize}
          aria-valuemax={maxSize}
          aria-valuenow={Math.round(size)}
          tabIndex={0}
          onDoubleClick={() => onSizeChange(defaultSize)}
          onKeyDown={resizeWithKeyboard}
          onPointerDown={resize}
        >
          {side === "bottom"
            ? <GripHorizontal aria-hidden="true" size={14} />
            : <GripVertical aria-hidden="true" size={14} />}
        </div>
      ) : null}
      <header className="shell-tab-bar resizable-dock-header">
        <div className="resizable-dock-header-content">{header}</div>
        <Collapsible.Trigger
          render={(
            <ShellPanelToggle
              side={side}
              expanded={open}
              label={label}
              shortcut={shortcut}
              shortcutKeys={shortcutKeys}
            />
          )}
        />
      </header>
      <Collapsible.Panel className="resizable-dock-panel" keepMounted>
        {children}
      </Collapsible.Panel>
    </Collapsible.Root>
  );
}

function clamp(value: number, minimum: number, maximum: number) {
  return Math.min(maximum, Math.max(minimum, value));
}
