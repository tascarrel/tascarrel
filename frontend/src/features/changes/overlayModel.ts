import type { shares } from "../../api/generated/index.ts";
import type { PresentedChangeKind } from "./changePresentation.tsx";

export function overlayChangeKind(
  change: shares.ShareOverlayChange,
): Extract<PresentedChangeKind, "Added" | "Deleted" | "Modified" | "Replaced"> {
  if (!change.baseKind) return "Added";
  if (!change.proposedKind) return "Deleted";
  return change.baseKind.tag === change.proposedKind.tag ? "Modified" : "Replaced";
}

export function overlayEntryDescription(change: shares.ShareOverlayChange): string {
  const before = change.baseKind?.tag;
  const after = change.proposedKind?.tag;
  if (!before && after) return `New ${after.toLowerCase()}`;
  if (before && !after) return `Deleted ${before.toLowerCase()}`;
  if (before && after && before !== after) {
    return `${before} replaced by ${after.toLowerCase()}`;
  }
  return `Modified ${after?.toLowerCase() ?? "entry"}`;
}

export function formatOverlaySize(value: string | number | undefined): string | undefined {
  if (value === undefined) return undefined;
  const bytes = Number(value);
  if (!Number.isFinite(bytes)) return `${String(value)} B`;
  if (bytes >= 1024 * 1024) return `${(bytes / (1024 * 1024)).toFixed(1)} MiB`;
  if (bytes >= 1024) return `${(bytes / 1024).toFixed(1)} KiB`;
  return `${String(bytes)} B`;
}

export function shortOverlayRevision(revision: shares.ShareOverlayRevision): string {
  return String(revision).slice(0, 8);
}
