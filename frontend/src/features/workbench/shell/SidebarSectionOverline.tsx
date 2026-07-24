import type { ReactNode } from "react";

export function SidebarSectionOverline({
  actions,
  children,
}: {
  actions?: ReactNode;
  children: ReactNode;
}) {
  return (
    <div className="sidebar-section-overline">
      <span>{children}</span>
      {actions ? <span className="sidebar-section-overline-actions">{actions}</span> : null}
    </div>
  );
}
