import type { HTMLAttributes } from "react";

import { cn } from "./classNames.ts";

type IconButtonGroupProps = Omit<HTMLAttributes<HTMLDivElement>, "aria-label" | "role"> & {
  label: string;
};

export function IconButtonGroup({ label, className, ...props }: IconButtonGroupProps) {
  return (
    <div
      {...props}
      className={cn("icon-button-group", className)}
      role="group"
      aria-label={label}
    />
  );
}
