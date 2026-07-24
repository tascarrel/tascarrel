import { cva, type VariantProps } from "class-variance-authority";
import type { HTMLAttributes } from "react";

import { cn } from "./classNames.ts";

export function CountBadge({
  count,
  className,
  size = "sm",
  tone = "primary",
  ...props
}: CountBadgeProps) {
  return (
    <span
      className={cn(countBadgeVariants({ size, tone }), className)}
      {...props}
    >
      {count}
    </span>
  );
}

export type CountBadgeProps = Omit<HTMLAttributes<HTMLSpanElement>, "children"> & VariantProps<typeof countBadgeVariants> & {
  count: number;
};
export type CountBadgeTone = NonNullable<CountBadgeProps["tone"]>;

const countBadgeVariants = cva(
  "inline-flex shrink-0 items-center justify-center rounded-full font-semibold leading-none text-canvas tabular-nums",
  {
    variants: {
      size: {
        xs: "h-3.5 min-w-3.5 px-1 text-[9px]",
        sm: "h-4 min-w-4 px-1 text-[10px]",
      },
      tone: {
        muted: "bg-muted",
        primary: "bg-accent",
        success: "bg-emerald-500",
        warning: "bg-amber-500",
        danger: "bg-red-500",
      },
    },
    defaultVariants: {
      size: "sm",
      tone: "primary",
    },
  },
);
