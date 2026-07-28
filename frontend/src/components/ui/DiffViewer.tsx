import {
  Component,
  type ErrorInfo,
  lazy,
  memo,
  type ReactNode,
  Suspense,
} from "react";

const UnifiedDiffContent = lazy(() =>
  import("./UnifiedDiffContent.tsx").then((module) => ({ default: module.UnifiedDiffContent })),
);

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
      <Suspense fallback={<pre className="overflow-x-auto p-3 text-xs leading-5 text-muted">{patch}</pre>}>
        <UnifiedDiffContent patch={normalizedPatch} />
      </Suspense>
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
