import { Popover as BasePopover } from "@base-ui/react/popover";
import type { ComponentProps, ReactNode } from "react";

import { cn } from "./classNames.ts";

type PopoverContentProps = Omit<
  ComponentProps<typeof BasePopover.Positioner>,
  "children" | "className" | "title"
> & {
  children: ReactNode;
  className?: string;
  description?: ReactNode;
  positionerClassName?: string;
  title: ReactNode;
  titleClassName?: string;
};

export const Popover = {
  Root: BasePopover.Root,
  Trigger: BasePopover.Trigger,
  Content: PopoverContent,
};

function PopoverContent({
  title,
  description,
  children,
  className,
  positionerClassName,
  titleClassName,
  side = "bottom",
  align = "start",
  sideOffset = 8,
  ...positionerProps
}: PopoverContentProps) {
  return (
    <BasePopover.Portal>
      <BasePopover.Positioner
        {...positionerProps}
        className={cn("app-popover-positioner", positionerClassName)}
        side={side}
        align={align}
        sideOffset={sideOffset}
      >
        <BasePopover.Popup className={cn("app-popover", className)}>
          <BasePopover.Title className={cn("app-popover-title", titleClassName)}>
            {title}
          </BasePopover.Title>
          {description ? (
            <BasePopover.Description className="app-popover-description">
              {description}
            </BasePopover.Description>
          ) : null}
          {children}
        </BasePopover.Popup>
      </BasePopover.Positioner>
    </BasePopover.Portal>
  );
}
