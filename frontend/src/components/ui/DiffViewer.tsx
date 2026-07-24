import {
  Component,
  type CSSProperties,
  type ErrorInfo,
  lazy,
  memo,
  type ReactNode,
  Suspense,
} from "react";

const PatchDiff = lazy(() =>
  import("@pierre/diffs/react").then((module) => ({ default: module.PatchDiff })),
);

const diffStyle: CSSProperties & Record<string, string> = {
  "--diffs-bg": "var(--color-surface)",
  "--diffs-bg-context": "var(--color-surface-raised)",
  "--diffs-bg-context-gutter": "var(--color-surface)",
  "--diffs-bg-separator": "var(--color-surface-raised)",
  "--diffs-fg": "var(--color-foreground)",
  "--diffs-fg-number": "var(--color-subtle)",
  "--diffs-added-dark": "var(--syntax-token-inserted)",
  "--diffs-deleted-dark": "var(--syntax-token-deleted)",
  "--diffs-modified-dark": "var(--color-accent-text)",
};

export const DiffViewer = memo(function DiffViewer({
  patch,
  fileName,
}: {
  patch: string;
  fileName?: string;
}) {
  const normalizedPatch = normalizePatch(patch, fileName);

  return (
    <DiffErrorBoundary key={normalizedPatch} patch={patch}>
      <div className="overflow-hidden rounded-xl border border-ui-border bg-surface-raised">
        <Suspense fallback={<pre className="overflow-x-auto p-3 text-xs leading-5 text-muted">{patch}</pre>}>
          <PatchDiff
            patch={normalizedPatch}
            disableWorkerPool
            style={diffStyle}
            options={{
              theme: "github-dark",
              diffStyle: "unified",
              diffIndicators: "bars",
              overflow: "scroll",
              lineDiffType: "word-alt",
              disableBackground: true,
            }}
          />
        </Suspense>
      </div>
    </DiffErrorBoundary>
  );
});

class DiffErrorBoundary extends Component<
  { children: ReactNode; patch: string },
  { failed: boolean }
> {
  state = { failed: false };

  static getDerivedStateFromError() {
    return { failed: true };
  }

  componentDidCatch(error: Error, info: ErrorInfo) {
    console.error("Failed to render unified diff", error, info);
  }

  render() {
    if (!this.state.failed) return this.props.children;
    return (
      <div className="overflow-hidden rounded-xl border border-amber-500/20 bg-amber-500/5">
        <div className="border-b border-amber-500/20 px-3 py-2 text-xs text-amber-300">
          This patch could not be parsed as a unified diff.
        </div>
        <pre className="overflow-x-auto p-3 text-xs leading-5 text-muted">{this.props.patch}</pre>
      </div>
    );
  }
}

function normalizePatch(patch: string, fileName?: string): string {
  const normalized = patch.replace(/\r\n?/g, "\n");
  if (/^(?:diff --git |--- [^\n]*\n\+\+\+ )/m.test(normalized)) return normalized;

  const path = normalizeFileName(fileName);
  return `--- a/${path}\n+++ b/${path}\n${normalized}`;
}

function normalizeFileName(fileName?: string): string {
  const path = fileName?.replace(/[\r\n]/g, "").replace(/^(?:a|b)\//, "").trim();
  return path || "file";
}
