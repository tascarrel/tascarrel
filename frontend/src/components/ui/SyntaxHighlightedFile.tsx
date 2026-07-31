import { File } from "@pierre/diffs/react";
import { useMemo } from "react";

import "./registerSidexLanguage.ts";

import { FULL_HEIGHT_DIFFS_CSS } from "./diffsLayout.ts";

const fileOptions = {
  theme: "github-dark",
  disableFileHeader: true,
  overflow: "scroll",
  unsafeCSS: FULL_HEIGHT_DIFFS_CSS,
} as const;

export function SyntaxHighlightedFile({
  contents,
  name,
  line,
}: {
  contents: string;
  name: string;
  line?: number;
}) {
  const file = useMemo(() => ({ contents, name }), [contents, name]);

  return (
    <File
      className="h-full min-h-0"
      disableWorkerPool
      file={file}
      options={fileOptions}
      selectedLines={line ? { start: line, end: line } : undefined}
    />
  );
}
