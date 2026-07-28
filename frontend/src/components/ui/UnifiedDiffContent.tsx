import { parsePatchFiles } from "@pierre/diffs";
import { FileDiff } from "@pierre/diffs/react";
import { useMemo } from "react";

import "./registerSidexLanguage.ts";

import { FULL_HEIGHT_DIFFS_CSS } from "./diffsLayout.ts";

const diffOptions = {
  theme: "github-dark",
  diffStyle: "unified",
  diffIndicators: "bars",
  overflow: "scroll",
  lineDiffType: "word-alt",
  disableBackground: true,
  unsafeCSS: FULL_HEIGHT_DIFFS_CSS,
} as const;

export function UnifiedDiffContent({ patch }: { patch: string }) {
  const files = useMemo(
    () => parsePatchFiles(patch).flatMap((parsedPatch) => parsedPatch.files),
    [patch],
  );

  if (!files.length) throw new Error("Unified diff contains no file changes");

  return (
    <div className="h-full min-h-0 divide-y divide-ui-border">
      {files.map((file, index) => (
        <FileDiff
          className="h-full min-h-0"
          disableWorkerPool
          fileDiff={file}
          key={file.cacheKey ?? `${file.prevName ?? ""}:${file.name}:${String(index)}`}
          options={diffOptions}
        />
      ))}
    </div>
  );
}
