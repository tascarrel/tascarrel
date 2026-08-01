import type { files } from "../../../api/generated/index.ts";

export type PodFileTarget = {
  root: files.FileRoot;
  path: string;
  line?: number;
};

/** Resolves a Markdown URL to a normalized path inside a pod file root. */
export function podFileTarget(
  href?: string,
  baseTarget?: PodFileTarget,
): PodFileTarget | undefined {
  const value = href?.trim();
  if (!value || value.startsWith("#")) return undefined;

  if (value.startsWith("file://")) {
    try {
      const url = new URL(value);
      if (url.hostname && url.hostname !== "localhost") return undefined;
      return normalizeLinkedFileTarget(
        decodePath(url.pathname),
        baseTarget,
        sourceLineFromHash(url.hash),
      );
    } catch {
      return undefined;
    }
  }
  if (/^[a-z][a-z\d+.-]*:/i.test(value)) return undefined;
  if (value.startsWith("//")) return undefined;

  const hashIndex = value.indexOf("#");
  const hash = hashIndex >= 0 ? value.slice(hashIndex) : "";
  const path = value.slice(0, hashIndex >= 0 ? hashIndex : undefined).split("?", 1)[0];
  return path
    ? normalizeLinkedFileTarget(decodePath(path), baseTarget, sourceLineFromHash(hash))
    : undefined;
}

function normalizeLinkedFileTarget(
  path: string,
  baseTarget?: PodFileTarget,
  hashLine?: number,
): PodFileTarget | undefined {
  let normalized = path.replace(/\\/g, "/");
  const suffix = normalized.match(/:(\d+)(?::\d+)?$/);
  const suffixLine = positiveLineNumber(suffix?.[1]);
  if (suffix) normalized = normalized.slice(0, -suffix[0].length);
  if (!normalized || normalized.endsWith("/") || normalized.includes("\0")) return undefined;

  let root = baseTarget?.root ?? WORKSPACE_ROOT;
  if (root.tag === "Share" && !validShareName(root.name)) return undefined;
  let rooted = false;
  if (normalized.startsWith("/workspace/")) {
    normalized = normalized.slice("/workspace/".length);
    root = WORKSPACE_ROOT;
    rooted = true;
  } else if (normalized.startsWith("/mnt/")) {
    const [share, ...components] = normalized.slice("/mnt/".length).split("/");
    if (!validShareName(share) || components.length === 0) return undefined;
    root = { tag: "Share", name: share };
    normalized = components.join("/");
    rooted = true;
  } else if (normalized.startsWith("/")) {
    return undefined;
  }

  const name = normalized.split("/").at(-1) ?? normalized;
  const likelyFile = rooted
    || normalized.startsWith("./")
    || normalized.startsWith("../")
    || normalized.includes("/")
    || name.includes(".")
    || /^(?:copying|dockerfile|justfile|license|makefile|procfile|readme)$/i.test(name);
  if (!likelyFile) return undefined;

  const base = rooted ? [] : fileDirectory(baseTarget?.path);
  if (!base) return undefined;
  for (const component of normalized.split("/")) {
    if (!component || component === ".") continue;
    if (component === "..") {
      if (!base.pop()) return undefined;
    } else {
      base.push(component);
    }
  }

  const resolvedPath = base.join("/");
  const line = hashLine ?? suffixLine;
  return resolvedPath
    ? { root, path: resolvedPath, ...(line ? { line } : {}) }
    : undefined;
}

const WORKSPACE_ROOT = { tag: "Workspace" } as const satisfies files.FileRoot;

function fileDirectory(path?: string): string[] | undefined {
  if (!path) return [];
  const normalized = path.replace(/\\/g, "/");
  if (normalized.startsWith("/") || normalized.endsWith("/") || normalized.includes("\0")) {
    return undefined;
  }
  const components = normalized.split("/");
  if (components.some((component) => !component || component === "." || component === "..")) {
    return undefined;
  }
  return components.slice(0, -1);
}

function validShareName(name: string | undefined): name is string {
  return Boolean(name && /^[A-Za-z0-9][A-Za-z0-9_-]{0,63}$/.test(name));
}

function decodePath(path: string): string {
  try {
    return decodeURIComponent(path);
  } catch {
    return path;
  }
}

function sourceLineFromHash(hash: string): number | undefined {
  return positiveLineNumber(hash.match(/^#?L(\d+)(?:-L\d+)?$/i)?.[1]);
}

function positiveLineNumber(value?: string): number | undefined {
  if (!value) return undefined;
  const line = Number(value);
  return Number.isSafeInteger(line) && line > 0 ? line : undefined;
}
