import type { InputHTMLAttributes } from "react";

/** Provides the standard single-line text field presentation. */
export function TextInput({
  className = "",
  ...props
}: InputHTMLAttributes<HTMLInputElement>) {
  return (
    <input
      className={`h-9 min-w-0 rounded-lg border border-ui-border bg-surface px-3 text-xs text-foreground outline-none transition placeholder:text-subtle hover:border-ui-border-strong focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-accent disabled:cursor-not-allowed disabled:opacity-50 aria-invalid:border-red-500/50 ${className}`}
      {...props}
    />
  );
}
