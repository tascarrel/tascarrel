import type { files, pods, workspaces } from "./generated/index.ts";
import { RAW_FILES_API_PATH } from "./paths.ts";

/** The persistent workspace root available to every pod. */
export const WORKSPACE_FILE_ROOT = { tag: "Workspace" } as const satisfies files.FileRoot;

/** Maximum UTF-8 byte length accepted by the lightweight editor. */
export const POD_TEXT_FILE_BYTE_LIMIT = 2 * 1024 * 1024;

/** Returns the authenticated HTTP data-plane URL for one pod file. */
export function podFileUrl(
  workspace: workspaces.WorkspaceName,
  podId: pods.PodId,
  root: files.FileRoot,
  path: files.FilePath,
  download = false,
): string {
  const url = new URL(RAW_FILES_API_PATH, window.location.href);
  url.searchParams.set("workspace", String(workspace));
  url.searchParams.set("podId", String(podId));
  if (root.tag === "Share") url.searchParams.set("share", root.name);
  url.searchParams.set("path", String(path));
  if (download) url.searchParams.set("download", "true");
  return url.href;
}

/** Replaces a pod text file if it still has the revision that was opened. */
export async function savePodTextFile(
  workspace: workspaces.WorkspaceName,
  podId: pods.PodId,
  root: files.FileRoot,
  path: files.FilePath,
  contents: string,
  expectedRevision: string,
): Promise<string> {
  const response = await fetch(podFileUrl(workspace, podId, root, path), {
    method: "PUT",
    headers: {
      "content-type": "text/plain; charset=utf-8",
      "if-match": `"${expectedRevision}"`,
      "x-tascarrel-request": "tascarrel-pod-file-write",
    },
    body: contents,
  });
  if (response.status === 412) {
    throw new PodFileConflictError();
  }
  if (!response.ok) {
    throw new Error(await responseError(response));
  }
  const revision = response.headers.get("etag")?.match(/^"([0-9a-f]{64})"$/)?.[1];
  if (!revision) throw new Error("The file was saved, but the server returned no revision.");
  return revision;
}

/** Raised when a file changed after it was opened for editing. */
export class PodFileConflictError extends Error {
  constructor() {
    super("The file changed after you opened it.");
    this.name = "PodFileConflictError";
  }
}

/** Returns the pod-visible absolute path for one file root. */
export function fileRootPath(root: files.FileRoot): string {
  return root.tag === "Workspace" ? "/workspace" : `/mnt/${root.name}`;
}

/** Returns the pod-visible absolute path for one root-relative file. */
export function podFilePath(root: files.FileRoot, path: string): string {
  return `${fileRootPath(root)}/${path}`;
}

/** Returns a stable client-side identity for one file root. */
export function fileRootKey(root: files.FileRoot): string {
  return root.tag === "Workspace" ? "workspace" : `share:${root.name}`;
}

async function responseError(response: Response): Promise<string> {
  const body = (await response.text()).trim();
  try {
    const parsed = JSON.parse(body) as { message?: unknown };
    if (typeof parsed.message === "string") return parsed.message;
  } catch {
    // Plain-text gateway diagnostics are suitable for display.
  }
  return body || `File write failed with status ${response.status}`;
}
