export type WorkspaceFileTarget = {
  path: string;
  line?: number;
};

/** Resolves a Markdown URL to a normalized path inside the pod workspace. */
export function workspaceFileTarget(
  href?: string,
  workspacePath?: string,
): WorkspaceFileTarget | undefined {
  const value = href?.trim();
  if (!value || value.startsWith("#")) return undefined;

  if (value.startsWith("file://")) {
    try {
      const url = new URL(value);
      if (url.hostname && url.hostname !== "localhost") return undefined;
      return normalizeLinkedFileTarget(
        decodePath(url.pathname),
        undefined,
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
    ? normalizeLinkedFileTarget(
        decodePath(path),
        workspacePath,
        sourceLineFromHash(hash),
      )
    : undefined;
}

/** Resolves a Markdown URL to a normalized workspace path without source metadata. */
export function workspaceFilePath(href?: string, workspacePath?: string): string | undefined {
  return workspaceFileTarget(href, workspacePath)?.path;
}

function normalizeLinkedFileTarget(
  path: string,
  workspacePath?: string,
  hashLine?: number,
): WorkspaceFileTarget | undefined {
  let normalized = path.replace(/\\/g, "/");
  const suffix = normalized.match(/:(\d+)(?::\d+)?$/);
  const suffixLine = positiveLineNumber(suffix?.[1]);
  if (suffix) normalized = normalized.slice(0, -suffix[0].length);
  if (!normalized || normalized.endsWith("/") || normalized.includes("\0")) return undefined;

  let rootedInWorkspace = false;
  if (normalized.startsWith("/workspace/")) {
    normalized = normalized.slice("/workspace/".length);
    rootedInWorkspace = true;
  } else if (normalized.startsWith("/")) {
    return undefined;
  }

  const name = normalized.split("/").at(-1) ?? normalized;
  const likelyFile = rootedInWorkspace
    || normalized.startsWith("./")
    || normalized.startsWith("../")
    || normalized.includes("/")
    || name.includes(".")
    || /^(?:copying|dockerfile|justfile|license|makefile|procfile|readme)$/i.test(name);
  if (!likelyFile) return undefined;

  const base = rootedInWorkspace ? [] : workspaceDirectory(workspacePath);
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
    ? { path: resolvedPath, ...(line ? { line } : {}) }
    : undefined;
}

function workspaceDirectory(workspacePath?: string): string[] | undefined {
  if (!workspacePath) return [];
  let normalized = workspacePath.replace(/\\/g, "/");
  if (normalized.startsWith("/workspace/")) {
    normalized = normalized.slice("/workspace/".length);
  } else if (normalized.startsWith("/")) {
    return undefined;
  }
  const components = normalized.split("/").filter(Boolean);
  if (components.some((component) => component === "." || component === "..")) {
    return undefined;
  }
  return components.slice(0, -1);
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
