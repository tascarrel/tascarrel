import { cva, type VariantProps } from "class-variance-authority";
import type { HTMLAttributes } from "react";

import { cn } from "./classNames.ts";

export function Badge({
  children,
  className,
  size = "sm",
  tone = "muted",
  ...props
}: BadgeProps) {
  return (
    <span
      className={cn(badgeVariants({ size, tone }), className)}
      {...props}
    >
      {children}
    </span>
  );
}

export type BadgeProps = HTMLAttributes<HTMLSpanElement> & VariantProps<typeof badgeVariants>;
export type BadgeTone = NonNullable<BadgeProps["tone"]>;

const badgeVariants = cva(
  "inline-flex items-center border font-medium",
  {
    variants: {
      size: {
        xs: "rounded-[5px] px-1.5 py-0.5 text-[9px] uppercase tracking-wide",
        sm: "rounded-[10px] px-2 py-1 text-[11px]",
      },
      tone: {
        muted: "border-ui-border bg-surface text-muted",
        primary: "border-accent/20 bg-accent/10 text-accent-text",
        success: "border-emerald-500/20 bg-emerald-500/10 text-emerald-300",
        warning: "border-amber-500/20 bg-amber-500/10 text-amber-300",
        danger: "border-red-500/20 bg-red-500/10 text-red-300",
      },
    },
    defaultVariants: {
      size: "sm",
      tone: "muted",
    },
  },
);
