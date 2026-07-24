import type { repositories } from "../../api/generated/index.ts";

export const DEFAULT_CODE_FOLDER = "/workspace";

/** Resolves one configured repository path inside a pod's workspace tree. */
export function repositoryCodeFolder(path: string): string {
  return `${DEFAULT_CODE_FOLDER}/${path.replace(/^\/+/, "")}`.replace(/\/$/, "");
}

/** Produces the compact tab label for an absolute Code session folder. */
export function codeFolderLabel(
  folder: string,
  configuredRepositories: readonly repositories.Repository[],
): string {
  if (folder === DEFAULT_CODE_FOLDER) return "Workspace";
  const relative = folder.startsWith(`${DEFAULT_CODE_FOLDER}/`)
    ? folder.slice(DEFAULT_CODE_FOLDER.length + 1)
    : folder;
  return configuredRepositories.find((repository) => repository.path === relative)?.path ?? relative;
}
