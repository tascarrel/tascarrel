import { Folder, FolderGit2 } from "lucide-react";
import { useMemo, useState } from "react";

import type { repositories } from "../../api/generated/index.ts";
import {
  CommandPalette,
  type CommandPaletteItem,
} from "../../components/ui/CommandPalette.tsx";
import { FuzzySearch } from "../../components/ui/FuzzySearch.tsx";
import { DEFAULT_CODE_FOLDER, repositoryCodeFolder } from "./folders.ts";

export function CodeDirectoryPalette({
  open,
  repositories,
  onOpenChange,
  onSelect,
}: {
  open: boolean;
  repositories: readonly repositories.Repository[];
  onOpenChange: (open: boolean) => void;
  onSelect: (folder: string) => void;
}) {
  const items = useCodeDirectoryItems(repositories);

  return (
    <CommandPalette
      open={open}
      title={TITLE}
      description={DESCRIPTION}
      items={items}
      placeholder={PLACEHOLDER}
      emptyMessage={EMPTY_MESSAGE}
      onOpenChange={onOpenChange}
      onSelect={onSelect}
    />
  );
}

export function CodeDirectoryPicker({
  repositories,
  onSelect,
}: {
  repositories: readonly repositories.Repository[];
  onSelect: (folder: string) => void;
}) {
  const items = useCodeDirectoryItems(repositories);
  const [query, setQuery] = useState("");

  return (
    <div className="flex h-full items-center justify-center bg-canvas px-6 py-10 text-foreground">
      <section className="w-full max-w-xl overflow-hidden rounded-xl border border-ui-border-strong bg-surface-raised shadow-2xl shadow-black/30">
        <div className="border-b border-divider px-4 py-3">
          <h1 className="text-sm font-semibold">{TITLE}</h1>
          <p className="mt-1 text-[11px] leading-4 text-subtle">{DESCRIPTION}</p>
        </div>
        <FuzzySearch
          items={items}
          query={query}
          searchLabel={`Search ${TITLE}`}
          placeholder={PLACEHOLDER}
          emptyMessage={EMPTY_MESSAGE}
          onQueryChange={setQuery}
          onSelect={onSelect}
        />
      </section>
    </div>
  );
}

function useCodeDirectoryItems(
  repositories: readonly repositories.Repository[],
): CommandPaletteItem<string>[] {
  return useMemo<CommandPaletteItem<string>[]>(() => [
    {
      id: "workspace-root",
      value: DEFAULT_CODE_FOLDER,
      label: "Workspace root",
      description: DEFAULT_CODE_FOLDER,
      keywords: ["root"],
      icon: <Folder aria-hidden="true" size={15} />,
    },
    ...repositories.map((repository) => {
      const folder = repositoryCodeFolder(repository.path);
      return {
        id: `repository-${repository.path}`,
        value: folder,
        label: repository.path,
        description: folder,
        keywords: [repository.source, "repository", "git"],
        icon: <FolderGit2 aria-hidden="true" size={15} />,
      };
    }),
  ], [repositories]);
}

const TITLE = "Open Code Session";
const DESCRIPTION = "Choose the working directory to open in a separate code-server session.";
const PLACEHOLDER = "Search workspace folders…";
const EMPTY_MESSAGE = "No matching workspace folder.";
