import type { files, pods, workspaces } from "./generated/index.ts";
import { RAW_FILES_API_PATH } from "./paths.ts";

/** Returns the authenticated HTTP data-plane URL for one pod workspace file. */
export function workspaceFileUrl(
  workspace: workspaces.WorkspaceName,
  podId: pods.PodId,
  path: files.FilePath,
  download = false,
): string {
  const url = new URL(RAW_FILES_API_PATH, window.location.href);
  url.searchParams.set("workspace", String(workspace));
  url.searchParams.set("podId", String(podId));
  url.searchParams.set("path", String(path));
  if (download) url.searchParams.set("download", "true");
  return url.href;
}
