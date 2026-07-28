import type { changes, pods } from "../../api/generated/index.ts";

export type PodChangeSummary = Readonly<{
  changedFileCount: number;
  dirtyRepositoryCount: number;
  conflictCount: number;
  unpushedCommitCount: number;
  repositoryWithoutUpstreamCount: number;
  inspectionFailureCount: number;
}>;

export function summarizePodChanges(
  repositories: readonly changes.RepositoryStatusEntry[],
): ReadonlyMap<pods.PodId, PodChangeSummary> {
  const summaries = new Map<pods.PodId, PodChangeSummary>();

  for (const repository of repositories) {
    const current = summaries.get(repository.target.podId);
    if (repository.state.status === "Failed") {
      summaries.set(repository.target.podId, {
        ...emptySummary(current),
        inspectionFailureCount: (current?.inspectionFailureCount ?? 0) + 1,
      });
      continue;
    }

    const working = repository.state.working;
    const changedFileCount = working.dirty
      ? numericCount(working.fileCount)
      : 0;
    const conflictCount = working.dirty
      ? numericCount(working.conflictCount)
      : 0;
    const unpushedCommitCount = repository.state.upstream
      ? numericCount(repository.state.upstream.ahead)
      : 0;
    const repositoryWithoutUpstreamCount = repository.state.head
      && !repository.state.upstream
      ? 1
      : 0;
    if (
      !working.dirty
      && unpushedCommitCount === 0
      && repositoryWithoutUpstreamCount === 0
    ) continue;

    summaries.set(repository.target.podId, {
      ...emptySummary(current),
      changedFileCount: (current?.changedFileCount ?? 0)
        + changedFileCount,
      dirtyRepositoryCount: (current?.dirtyRepositoryCount ?? 0)
        + (working.dirty ? 1 : 0),
      conflictCount: (current?.conflictCount ?? 0)
        + conflictCount,
      unpushedCommitCount: (current?.unpushedCommitCount ?? 0)
        + unpushedCommitCount,
      repositoryWithoutUpstreamCount: (current?.repositoryWithoutUpstreamCount ?? 0)
        + repositoryWithoutUpstreamCount,
    });
  }

  return summaries;
}

function emptySummary(summary: PodChangeSummary | undefined): PodChangeSummary {
  return summary ?? {
    changedFileCount: 0,
    dirtyRepositoryCount: 0,
    conflictCount: 0,
    unpushedCommitCount: 0,
    repositoryWithoutUpstreamCount: 0,
    inspectionFailureCount: 0,
  };
}

function numericCount(value: string | number): number {
  const count = Number(value);
  if (!Number.isFinite(count) || count <= 0) return 0;
  return Math.min(Math.floor(count), Number.MAX_SAFE_INTEGER);
}
