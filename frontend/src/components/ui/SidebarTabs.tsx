import { Tabs } from "@base-ui/react/tabs";
import type { ComponentProps, ReactNode } from "react";

import { cn } from "./classNames.ts";

const SIDEBAR_TAB_CLASS = "flex w-full rounded-lg px-3 py-2 text-left text-xs font-medium text-muted outline-none transition hover:bg-surface hover:text-foreground focus-visible:outline-2 focus-visible:outline-accent";

export type SidebarTabItem = {
  value: string;
  label: string;
  tone?: "default" | "danger";
};

export function SidebarTabs({
  ariaLabel,
  defaultValue,
  items,
  children,
}: {
  ariaLabel: string;
  defaultValue: string;
  items: readonly SidebarTabItem[];
  children: ReactNode;
}) {
  return (
    <Tabs.Root
      className="grid min-h-0 flex-1 grid-cols-[10rem_minmax(0,1fr)] overflow-hidden sm:grid-cols-[13rem_minmax(0,1fr)]"
      defaultValue={defaultValue}
      orientation="vertical"
    >
      <Tabs.List className="min-h-0 p-4 sm:p-6" aria-label={ariaLabel}>
        {items.map((item) => (
          <Tabs.Tab
            className={cn(
              SIDEBAR_TAB_CLASS,
              item.tone === "danger"
                ? "data-[active]:bg-red-500/5 data-[active]:text-red-200"
                : "data-[active]:bg-surface-raised data-[active]:text-accent-text",
            )}
            key={item.value}
            value={item.value}
          >
            {item.label}
          </Tabs.Tab>
        ))}
      </Tabs.List>
      {children}
    </Tabs.Root>
  );
}

export function SidebarTabsPanel({
  children,
  className,
  contentClassName,
  ...props
}: ComponentProps<typeof Tabs.Panel> & { contentClassName?: string }) {
  return (
    <Tabs.Panel
      className={cn("min-h-0 overflow-y-auto p-4 outline-none sm:p-6", className)}
      {...props}
    >
      {contentClassName ? <div className={contentClassName}>{children}</div> : children}
    </Tabs.Panel>
  );
}
