import type { shares } from "../../api/generated/index.ts";
import { Badge } from "../../components/ui/Badge.tsx";
import { DiffViewer } from "../../components/ui/DiffViewer.tsx";
import {
  formatOverlaySize,
  overlayChangeKind,
  overlayEntryDescription,
} from "./overlayModel.ts";

export function OverlayChangeSetSummary({ approval }: { approval: shares.ShareOverlayApprovalRequest }) {
  const counts = new Map<string, number>();
  for (const change of approval.changes) {
    const kind = overlayChangeKind(change);
    counts.set(kind, (counts.get(kind) ?? 0) + 1);
  }
  return (
    <div className="mx-auto max-w-xl">
      <h3 className="text-sm font-semibold text-foreground">Submitted overlay revision</h3>
      <p className="mt-2 text-xs leading-5 text-subtle">
        This overlay has no commit history. The approval is bound directly to the complete captured filesystem revision shown here.
      </p>
      <dl className="mt-4 grid grid-cols-2 gap-2 sm:grid-cols-4">
        {["Added", "Modified", "Deleted", "Replaced"].map((kind) => (
          <div className="rounded-lg border border-ui-border bg-surface/50 p-3" key={kind}>
            <dt className="text-[10px] text-subtle">{kind}</dt>
            <dd className="mt-1 font-mono text-sm text-foreground">{String(counts.get(kind) ?? 0)}</dd>
          </div>
        ))}
      </dl>
      <p className="mt-4 rounded-lg border border-ui-border bg-surface/40 px-3 py-2 text-[11px] leading-5 text-muted">
        Select a path to inspect its entry type, proposed size, and a unified patch when bounded UTF-8 text is available.
      </p>
    </div>
  );
}

export function OverlayChangeDetail({ change }: { change: shares.ShareOverlayChange }) {
  return (
    <div className="mx-auto max-w-xl">
      <div className="flex flex-wrap items-start justify-between gap-3">
        <div className="min-w-0">
          <p className="break-all font-mono text-xs font-semibold text-foreground">{change.path}</p>
          <p className="mt-1 text-[11px] text-subtle">{overlayEntryDescription(change)}</p>
        </div>
        <Badge tone={overlayChangeKind(change) === "Deleted" ? "danger" : "warning"}>
          {overlayChangeKind(change)}
        </Badge>
      </div>
      <dl className="mt-5 grid grid-cols-[auto_minmax(0,1fr)] gap-x-4 gap-y-2 rounded-lg border border-ui-border bg-surface/50 p-3 text-xs">
        <dt className="text-subtle">Existing entry</dt>
        <dd className="text-muted">{change.baseKind?.tag ?? "None"}</dd>
        <dt className="text-subtle">Proposed entry</dt>
        <dd className="text-muted">{change.proposedKind?.tag ?? "None"}</dd>
        {change.proposedSize === undefined ? null : (
          <>
            <dt className="text-subtle">Proposed size</dt>
            <dd className="text-muted">{formatOverlaySize(change.proposedSize)}</dd>
          </>
        )}
      </dl>
      {change.textDiff === undefined ? (
        <p className="mt-4 rounded-lg border border-ui-border bg-surface/40 px-3 py-2 text-[11px] leading-5 text-subtle">
          No text patch is available for this structural, binary, or large-file change. The decision remains bound to the exact captured revision.
        </p>
      ) : change.textDiff ? (
        <div className="mt-4 overflow-hidden rounded-lg border border-ui-border bg-canvas">
          <DiffViewer patch={change.textDiff} fileName={change.path} />
        </div>
      ) : (
        <p className="mt-4 text-[11px] leading-5 text-subtle">
          The regular-file contents are unchanged; this proposal changes filesystem metadata only.
        </p>
      )}
    </div>
  );
}

export function OverlayChangeMetadata({
  change,
  compact = false,
}: {
  change: shares.ShareOverlayChange;
  compact?: boolean;
}) {
  const lines = overlayDiffLines(change.textDiff);
  if (lines) {
    return (
      <span className={`shrink-0 font-mono ${compact ? "text-[9px]" : "text-[10px]"}`}>
        <span className="text-emerald-300">+{String(lines.additions)}</span>{" "}
        <span className="text-red-300">−{String(lines.deletions)}</span>
      </span>
    );
  }
  const size = formatOverlaySize(change.proposedSize);
  return size
    ? <span className={`shrink-0 font-mono text-subtle ${compact ? "text-[9px]" : "text-[10px]"}`}>{size}</span>
    : null;
}

function overlayDiffLines(diff: string | undefined): { additions: number; deletions: number } | undefined {
  if (!diff) return undefined;
  let additions = 0;
  let deletions = 0;
  for (const line of diff.split("\n")) {
    if (line.startsWith("+") && !line.startsWith("+++ ")) additions += 1;
    if (line.startsWith("-") && !line.startsWith("--- ")) deletions += 1;
  }
  return { additions, deletions };
}
