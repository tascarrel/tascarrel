import type { files, pods, workspaces } from "./generated/index.ts";
import { RAW_FILES_API_PATH } from "./paths.ts";

/** The persistent workspace root available to every pod. */
export const WORKSPACE_FILE_ROOT = { tag: "Workspace" } as const satisfies files.FileRoot;

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
