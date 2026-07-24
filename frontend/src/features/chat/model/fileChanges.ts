import type { chats } from "../../../api/generated/index.ts";

export interface FilePatch {
  patch: string;
  fileName?: string;
}

export interface LineChangePresentation {
  additions: string;
  deletions: string;
  description: string;
}

interface LineChanges {
  additions: bigint;
  deletions: bigint;
}

interface Replacement {
  before: string;
  after: string;
}

export function findFilePatches(content: chats.ChatContent[]): FilePatch[] {
  const structured = content.flatMap((part) =>
    part.kind === "Structured" ? findStructuredFilePatches(part.value) : [],
  );
  if (structured.length > 0) return uniquePatches(structured);

  return uniquePatches(
    content.flatMap((part) =>
      part.kind === "Text" && looksLikePatch(part.value) ? [{ patch: part.value }] : [],
    ),
  );
}

export function findStructuredFilePatches(
  value: unknown,
  inheritedFileName?: string,
  depth = 0,
): FilePatch[] {
  if (depth > 4 || !value || typeof value !== "object") return [];
  if (Array.isArray(value)) {
    return value.flatMap((child) =>
      findStructuredFilePatches(child, inheritedFileName, depth + 1),
    );
  }

  const record = value as Record<string, unknown>;
  const fileName = stringField(record, "path", "filePath", "file", "file_path")
    ?? inheritedFileName;
  const direct = ["patch", "diff", "unifiedDiff", "unified_diff"].flatMap((key) => {
    const candidate = record[key];
    return typeof candidate === "string" && looksLikePatch(candidate)
      ? [{ patch: candidate, fileName }]
      : [];
  });
  if (direct.length > 0) return direct;

  return Object.values(record).flatMap((child) =>
    findStructuredFilePatches(child, fileName, depth + 1),
  );
}

export function presentChatLineChanges(
  timeline: chats.ChatTimelineEntry[],
): LineChangePresentation | undefined {
  let additions = 0n;
  let deletions = 0n;
  let hasLineChanges = false;

  for (const entry of timeline) {
    if (
      entry.entry !== "Item"
      || entry.kind !== "FileChange"
      || entry.state !== "Completed"
    ) {
      continue;
    }
    const changes = lineChangesFromContent(entry.content);
    if (!changes) continue;
    hasLineChanges = true;
    additions += changes.additions;
    deletions += changes.deletions;
  }

  if (!hasLineChanges) return undefined;
  return {
    additions: formatCompactNumber(additions),
    deletions: formatCompactNumber(deletions),
    description: [
      `${formatNumber(additions)} ${pluralize(additions, "line")} added`,
      `${formatNumber(deletions)} ${pluralize(deletions, "line")} removed`,
    ].join(" · "),
  };
}

function lineChangesFromContent(content: chats.ChatContent[]): LineChanges | undefined {
  const patches = findFilePatches(content);
  if (patches.length > 0) {
    return sumLineChanges(patches.map(({ patch }) => countPatchLineChanges(patch)));
  }

  const summaries = content.flatMap((part) =>
    part.kind === "Structured" ? findStructuredLineChanges(part.value) : [],
  );
  if (summaries.length > 0) return sumLineChanges(summaries);

  const replacements = content.flatMap((part) =>
    part.kind === "Structured" ? findReplacements(part.value) : [],
  );
  if (replacements.length === 0) return undefined;
  return sumLineChanges(
    uniqueReplacements(replacements)
      .map(({ before, after }) => countReplacementLineChanges(before, after)),
  );
}

function countPatchLineChanges(patch: string): LineChanges {
  let additions = 0n;
  let deletions = 0n;
  let inHunk = false;

  for (const rawLine of patch.split("\n")) {
    const line = rawLine.endsWith("\r") ? rawLine.slice(0, -1) : rawLine;
    if (line.startsWith("@@ ")) {
      inHunk = true;
      continue;
    }
    if (line.startsWith("diff --git ")) {
      inHunk = false;
      continue;
    }
    if (!inHunk) continue;
    if (line.startsWith("+")) additions += 1n;
    if (line.startsWith("-")) deletions += 1n;
  }

  return { additions, deletions };
}

function findStructuredLineChanges(value: unknown, depth = 0): LineChanges[] {
  if (depth > 4 || !value || typeof value !== "object") return [];
  if (Array.isArray(value)) {
    return value.flatMap((child) => findStructuredLineChanges(child, depth + 1));
  }

  const record = value as Record<string, unknown>;
  const direct = lineChanges(record);
  if (direct) return [direct];
  const summary = asRecord(record.summary);
  const summarized = summary ? lineChanges(summary) : undefined;
  if (summarized) return [summarized];
  return Object.values(record).flatMap((child) => findStructuredLineChanges(child, depth + 1));
}

function findReplacements(value: unknown, depth = 0): Replacement[] {
  if (depth > 5 || !value || typeof value !== "object") return [];
  if (Array.isArray(value)) {
    return value.flatMap((child) => findReplacements(child, depth + 1));
  }

  const record = value as Record<string, unknown>;
  const before = stringField(record, "old_string", "oldString");
  const after = stringField(record, "new_string", "newString");
  if (before !== undefined && after !== undefined) return [{ before, after }];
  return Object.values(record).flatMap((child) => findReplacements(child, depth + 1));
}

function countReplacementLineChanges(before: string, after: string): LineChanges {
  const beforeLines = splitLines(before);
  const afterLines = splitLines(after);
  let start = 0;
  while (
    start < beforeLines.length
    && start < afterLines.length
    && beforeLines[start] === afterLines[start]
  ) {
    start += 1;
  }

  let beforeEnd = beforeLines.length;
  let afterEnd = afterLines.length;
  while (
    beforeEnd > start
    && afterEnd > start
    && beforeLines[beforeEnd - 1] === afterLines[afterEnd - 1]
  ) {
    beforeEnd -= 1;
    afterEnd -= 1;
  }

  const removed = beforeLines.slice(start, beforeEnd);
  const added = afterLines.slice(start, afterEnd);
  const common = longestCommonSubsequenceLength(removed, added);
  return {
    additions: BigInt(added.length - common),
    deletions: BigInt(removed.length - common),
  };
}

function longestCommonSubsequenceLength(before: string[], after: string[]): number {
  if (before.length === 0 || after.length === 0) return 0;
  // Replacement strings are normally short. Avoid quadratic work for an unusually large write.
  if (before.length * after.length > 1_000_000) return 0;

  const lengths = new Uint32Array(after.length + 1);
  for (const beforeLine of before) {
    let diagonal = 0;
    for (let index = 1; index <= after.length; index += 1) {
      const above = lengths[index];
      lengths[index] = beforeLine === after[index - 1]
        ? diagonal + 1
        : Math.max(lengths[index], lengths[index - 1]);
      diagonal = above;
    }
  }
  return lengths[after.length];
}

function splitLines(value: string): string[] {
  if (value.length === 0) return [];
  const lines = value.replaceAll("\r\n", "\n").split("\n");
  if (lines.at(-1) === "") lines.pop();
  return lines;
}

function lineChanges(record: Record<string, unknown>): LineChanges | undefined {
  const additions = nonNegativeBigInt(record.additions);
  const deletions = nonNegativeBigInt(record.deletions);
  return additions !== undefined && deletions !== undefined ? { additions, deletions } : undefined;
}

function nonNegativeBigInt(value: unknown): bigint | undefined {
  if ((typeof value !== "string" && typeof value !== "number" && typeof value !== "bigint")
    || String(value).startsWith("-")) {
    return undefined;
  }
  try {
    return BigInt(value);
  } catch {
    return undefined;
  }
}

function sumLineChanges(changes: LineChanges[]): LineChanges {
  return changes.reduce(
    (total, change) => ({
      additions: total.additions + change.additions,
      deletions: total.deletions + change.deletions,
    }),
    { additions: 0n, deletions: 0n },
  );
}

function uniquePatches(patches: FilePatch[]): FilePatch[] {
  const seen = new Set<string>();
  return patches.filter(({ patch, fileName }) => {
    const key = `${fileName ?? ""}\0${patch}`;
    if (seen.has(key)) return false;
    seen.add(key);
    return true;
  });
}

function uniqueReplacements(replacements: Replacement[]): Replacement[] {
  const seen = new Set<string>();
  return replacements.filter(({ before, after }) => {
    const key = `${before}\0${after}`;
    if (seen.has(key)) return false;
    seen.add(key);
    return true;
  });
}

function looksLikePatch(value: string): boolean {
  return /(^diff --git |^@@ -\d|^--- .+\n\+\+\+ )/m.test(value);
}

function asRecord(value: unknown): Record<string, unknown> | undefined {
  return value && typeof value === "object" && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : undefined;
}

function stringField(record: Record<string, unknown>, ...keys: string[]): string | undefined {
  for (const key of keys) {
    const value = record[key];
    if (typeof value === "string") return value;
  }
  return undefined;
}

function formatCompactNumber(value: bigint): string {
  return new Intl.NumberFormat(undefined, {
    notation: "compact",
    maximumFractionDigits: 1,
  }).format(value);
}

function formatNumber(value: bigint): string {
  return new Intl.NumberFormat().format(value);
}

function pluralize(value: bigint, singular: string): string {
  return value === 1n ? singular : `${singular}s`;
}
