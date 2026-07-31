import { ChevronRight } from "lucide-react";
import type { ReactNode } from "react";

import { Badge } from "../../components/ui/Badge.tsx";

export type PresentedChangeKind =
  | "Added"
  | "Deleted"
  | "Modified"
  | "Replaced"
  | "Renamed"
  | "Copied"
  | "TypeChanged"
  | "Unmerged"
  | "Untracked";

export function ChangeSourceRow({
  icon,
  title,
  subtitle,
  count,
  countTone = "warning",
  selected,
  onSelect,
}: {
  icon: ReactNode;
  title: string;
  subtitle: string;
  count: number;
  countTone?: "warning" | "danger" | "primary" | "muted";
  selected: boolean;
  onSelect: () => void;
}) {
  return (
    <button
      aria-pressed={selected}
      className="mb-1 flex w-full min-w-0 items-center gap-2 rounded-lg border border-transparent px-2.5 py-2 text-left outline-none transition hover:border-ui-border hover:bg-surface focus-visible:outline-2 focus-visible:outline-accent data-[selected=true]:border-ui-border-strong data-[selected=true]:bg-surface-raised"
      data-selected={selected}
      type="button"
      onClick={onSelect}
    >
      <span className="shrink-0 text-subtle">{icon}</span>
      <span className="min-w-0 flex-1">
        <span className="block truncate font-mono text-[11px] text-foreground">{title}</span>
        <span className="mt-0.5 block truncate text-[10px] text-subtle">{subtitle}</span>
      </span>
      <Badge size="xs" tone={countTone}>{String(count)}</Badge>
      <ChevronRight aria-hidden="true" className="size-3 shrink-0 text-subtle" />
    </button>
  );
}

export function ChangedFileRow({
  path,
  kind,
  metadata,
  selected,
  mobile = false,
  onSelect,
}: {
  path: string;
  kind: PresentedChangeKind;
  metadata?: ReactNode;
  selected?: boolean;
  mobile?: boolean;
  onSelect: () => void;
}) {
  return (
    <button
      aria-pressed={selected}
      className={mobile
        ? "flex min-h-14 w-full items-center gap-3 rounded-xl border border-ui-border bg-surface/70 p-3 text-left outline-none active:bg-surface-raised focus-visible:outline-2 focus-visible:outline-accent"
        : "mb-0.5 flex w-full min-w-0 items-center gap-2 rounded-lg px-2.5 py-1.5 text-left text-[11px] text-muted outline-none hover:bg-surface focus-visible:outline-2 focus-visible:outline-accent data-[selected=true]:bg-accent/10 data-[selected=true]:text-accent-text"}
      data-selected={selected}
      title={path}
      type="button"
      onClick={onSelect}
    >
      <ChangeKindMarker kind={kind} mobile={mobile} />
      <span className={`min-w-0 flex-1 truncate ${mobile ? "font-mono text-[11px] text-foreground" : ""}`}>
        {path}
      </span>
      {metadata}
      {mobile ? <ChevronRight aria-hidden="true" className="size-4 shrink-0 text-subtle" /> : null}
    </button>
  );
}

export function ChangeKindMarker({
  kind,
  mobile = false,
}: {
  kind: PresentedChangeKind;
  mobile?: boolean;
}) {
  const label = kind === "Untracked" ? "U" : kind.slice(0, 1);
  const tone = kind === "Deleted" || kind === "Unmerged"
    ? "text-red-300"
    : kind === "Added" || kind === "Untracked"
      ? "text-emerald-300"
      : "text-amber-300";
  return (
    <span
      aria-label={kind}
      className={`${mobile ? "grid size-7 place-items-center rounded-lg bg-surface-raised" : "w-3"} shrink-0 font-mono text-[10px] ${tone}`}
      title={kind}
    >
      {label}
    </span>
  );
}
