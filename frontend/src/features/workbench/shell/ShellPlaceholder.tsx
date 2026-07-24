import type { LucideIcon } from "lucide-react";

export function ShellPlaceholder({
  icon: Icon,
  title,
  detail,
}: {
  icon: LucideIcon;
  title: string;
  detail: string;
}) {
  return (
    <div className="workbench-empty">
      <span><Icon aria-hidden="true" size={19} /></span>
      <strong>{title}</strong>
      <p>{detail}</p>
    </div>
  );
}
