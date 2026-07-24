import { RAW_FILES_API_PATH } from "./paths.ts";

export function localFileDownloadUrl(workspace: string, podId: string, path: string): string {
  const url = new URL(RAW_FILES_API_PATH, window.location.href);
  url.searchParams.set("workspace", workspace);
  url.searchParams.set("podId", podId);
  url.searchParams.set("path", path);
  url.searchParams.set("download", "true");
  return url.toString();
}
