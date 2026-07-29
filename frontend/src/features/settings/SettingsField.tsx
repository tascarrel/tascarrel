import type { ReactNode } from "react";

/** Presents a consistently labeled field in workspace settings editors. */
export function SettingsField({
  label,
  children,
}: {
  label: string;
  children: ReactNode;
}) {
  return (
    <label className="flex min-w-0 flex-col gap-1">
      <span className="text-[10px] text-subtle">{label}</span>
      {children}
    </label>
  );
}
