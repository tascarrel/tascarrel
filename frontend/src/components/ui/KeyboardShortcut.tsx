const KEY_SYMBOLS: Record<string, string> = {
  Alt: "⌥",
  Control: "⌘",
  Ctrl: "⌘",
  Enter: "↵",
  Meta: "⌘",
  Mod: "⌘",
  Shift: "⇧",
};

const KEY_NAMES: Record<string, string> = {
  Alt: "Alt",
  Control: "Control",
  Ctrl: "Control",
  Enter: "Enter",
  Meta: "Command",
  Mod: "Control or Command",
  Shift: "Shift",
};

export function formatKeyboardShortcut(keys: ReadonlyArray<string>): string {
  return keys.map((key) => KEY_SYMBOLS[key] ?? key).join("");
}

export function KeyboardShortcut({
  keys,
  className,
}: {
  keys: ReadonlyArray<string>;
  className?: string;
}) {
  return (
    <kbd
      className={`keyboard-shortcut ${className ?? ""}`}
      aria-label={keys.map((key) => KEY_NAMES[key] ?? key).join(" plus ")}
    >
      <span aria-hidden="true">{formatKeyboardShortcut(keys)}</span>
    </kbd>
  );
}
