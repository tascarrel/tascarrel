import type { ButtonHTMLAttributes, ReactNode } from "react";

export function Button({
  children,
  className = "",
  size = "default",
  variant = "muted",
  type = "button",
  ...props
}: ButtonHTMLAttributes<HTMLButtonElement> & {
  size?: "default" | "small" | "icon";
  variant?: "muted" | "primary" | "danger";
  children: ReactNode;
}) {
  return (
    <button
      className={`inline-flex items-center justify-center gap-2 rounded-lg font-medium outline-none transition focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-accent disabled:cursor-not-allowed disabled:opacity-40 ${
        size === "icon" ? "size-9" : size === "small" ? "h-8 px-2.5 text-xs" : "h-9 px-3 text-xs"
      } ${
        variant === "primary"
          ? "bg-accent text-white enabled:hover:bg-accent-hover"
          : variant === "danger"
            ? "border border-red-500/25 bg-red-500/5 text-red-200 enabled:hover:bg-red-500/10"
            : "border border-ui-border/70 bg-surface text-muted enabled:hover:border-ui-border-strong enabled:hover:bg-surface-raised enabled:hover:text-foreground"
      } ${className}`}
      type={type}
      {...props}
    >
      {children}
    </button>
  );
}
