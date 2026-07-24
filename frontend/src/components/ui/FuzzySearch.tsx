import { Search } from "lucide-react";
import {
  type KeyboardEvent,
  type ReactNode,
  useEffect,
  useId,
  useMemo,
  useRef,
  useState,
} from "react";

export type FuzzySearchItem<Value> = {
  id: string;
  value: Value;
  label: string;
  description?: string;
  keywords?: readonly string[];
  icon?: ReactNode;
  trailing?: ReactNode;
  disabled?: boolean;
};

/** Renders a keyboard-navigable list ranked by fuzzy text matching. */
export function FuzzySearch<Value>({
  items,
  query,
  searchLabel,
  placeholder,
  emptyMessage,
  onQueryChange,
  onSelect,
}: {
  items: readonly FuzzySearchItem<Value>[];
  query: string;
  searchLabel: string;
  placeholder: string;
  emptyMessage: string;
  onQueryChange: (query: string) => void;
  onSelect: (value: Value) => void;
}) {
  const listId = useId();
  const listRef = useRef<HTMLDivElement>(null);
  const [activeIndex, setActiveIndex] = useState(0);
  const results = useMemo(() => fuzzySearch(items, query), [items, query]);
  const selectedIndex = selectableIndex(results, activeIndex);
  const activeItem = selectedIndex === undefined ? undefined : results[selectedIndex];

  useEffect(() => setActiveIndex(0), [query]);

  useEffect(() => {
    listRef.current
      ?.querySelector<HTMLElement>('[data-active="true"]')
      ?.scrollIntoView({ block: "nearest" });
  }, [activeItem?.id]);

  const handleKeyDown = (event: KeyboardEvent<HTMLInputElement>) => {
    if (results.length === 0) return;
    if (event.key === "ArrowDown") {
      event.preventDefault();
      setActiveIndex((current) => nextSelectableIndex(results, current, 1));
    } else if (event.key === "ArrowUp") {
      event.preventDefault();
      setActiveIndex((current) => nextSelectableIndex(results, current, -1));
    } else if (event.key === "Enter") {
      event.preventDefault();
      const item = selectedIndex === undefined ? undefined : results[selectedIndex];
      if (item && !item.disabled) onSelect(item.value);
    }
  };

  return (
    <div className="min-h-0">
      <div className="flex h-11 items-center gap-2.5 border-b border-divider px-3.5 text-subtle focus-within:text-muted">
        <Search aria-hidden="true" className="size-3.5 shrink-0" />
        <input
          autoFocus
          className="min-w-0 flex-1 border-0 bg-transparent text-sm text-foreground outline-none placeholder:text-subtle"
          role="combobox"
          aria-activedescendant={activeItem ? `${listId}-${activeItem.id}` : undefined}
          aria-autocomplete="list"
          aria-controls={listId}
          aria-expanded="true"
          aria-label={searchLabel}
          placeholder={placeholder}
          value={query}
          onChange={(event) => onQueryChange(event.target.value)}
          onKeyDown={handleKeyDown}
        />
      </div>
      <div
        className="max-h-80 overflow-y-auto p-1.5"
        id={listId}
        ref={listRef}
        role="listbox"
        aria-label="Search results"
      >
        {results.length > 0 ? results.map((item, index) => {
          const active = index === selectedIndex;
          return (
            <button
              className="flex w-full items-center gap-3 rounded-lg px-3 py-2.5 text-left text-muted outline-none transition-colors hover:bg-surface-hover hover:text-foreground data-[active]:bg-surface-active data-[active]:text-foreground disabled:cursor-default disabled:opacity-40 disabled:hover:bg-transparent disabled:hover:text-muted"
              data-active={active || undefined}
              disabled={item.disabled}
              id={`${listId}-${item.id}`}
              key={item.id}
              role="option"
              tabIndex={-1}
              type="button"
              aria-disabled={item.disabled || undefined}
              aria-selected={active}
              onClick={() => onSelect(item.value)}
              onMouseDown={(event) => event.preventDefault()}
              onPointerMove={() => {
                if (!item.disabled) setActiveIndex(index);
              }}
            >
              {item.icon ? (
                <span className="flex size-7 shrink-0 items-center justify-center text-subtle">
                  {item.icon}
                </span>
              ) : null}
              <span className="min-w-0 flex-1">
                <strong className="block truncate text-xs font-medium text-inherit">{item.label}</strong>
                {item.description ? (
                  <span className="mt-0.5 block truncate font-mono text-[10px] text-subtle">
                    {item.description}
                  </span>
                ) : null}
              </span>
              {item.trailing ? <span className="shrink-0">{item.trailing}</span> : null}
            </button>
          );
        }) : (
          <div className="px-3 py-8 text-center text-xs text-subtle" role="status">
            {emptyMessage}
          </div>
        )}
      </div>
    </div>
  );
}

function selectableIndex<Value>(
  items: readonly FuzzySearchItem<Value>[],
  preferred: number,
): number | undefined {
  if (items[preferred] && !items[preferred].disabled) return preferred;
  const index = items.findIndex((item) => !item.disabled);
  return index < 0 ? undefined : index;
}

function nextSelectableIndex<Value>(
  items: readonly FuzzySearchItem<Value>[],
  current: number,
  direction: -1 | 1,
): number {
  if (items.length === 0) return 0;
  for (let offset = 1; offset <= items.length; offset += 1) {
    const index = (current + direction * offset + items.length) % items.length;
    if (!items[index]?.disabled) return index;
  }
  return current;
}

/** Ranks items by exact, substring, and ordered-character matches. */
export function fuzzySearch<Value>(
  items: readonly FuzzySearchItem<Value>[],
  query: string,
): FuzzySearchItem<Value>[] {
  const terms = normalize(query).split(/\s+/).filter(Boolean);
  if (terms.length === 0) return [...items];
  return items
    .map((item, index) => {
      const candidate = normalize([
        item.label,
        item.description,
        ...(item.keywords ?? []),
      ].filter(Boolean).join(" "));
      const scores = terms.map((term) => fuzzyMatchScore(term, candidate));
      return {
        item,
        index,
        score: scores.every((score) => score !== undefined)
          ? scores.reduce<number>((total, score) => total + (score ?? 0), 0)
          : undefined,
      };
    })
    .filter((result): result is typeof result & { score: number } => result.score !== undefined)
    .toSorted((left, right) => right.score - left.score || left.index - right.index)
    .map((result) => result.item);
}

function fuzzyMatchScore(query: string, candidate: string): number | undefined {
  if (candidate === query) return 10_000;
  const substring = candidate.indexOf(query);
  if (substring >= 0) {
    const boundaryBonus = substring === 0 || !isWordCharacter(candidate[substring - 1]) ? 400 : 0;
    return 5_000 + boundaryBonus - substring * 4 - candidate.length * 0.01;
  }

  let candidateIndex = 0;
  let previousMatch = -2;
  let score = 0;
  for (const character of query) {
    const match = candidate.indexOf(character, candidateIndex);
    if (match < 0) return undefined;
    const consecutive = match === previousMatch + 1;
    const boundary = match === 0 || !isWordCharacter(candidate[match - 1]);
    score += 20 + (consecutive ? 18 : 0) + (boundary ? 14 : 0) - (match - candidateIndex);
    previousMatch = match;
    candidateIndex = match + 1;
  }
  return score - candidate.length * 0.01;
}

function normalize(value: string): string {
  return value.toLocaleLowerCase();
}

function isWordCharacter(character: string | undefined): boolean {
  return character !== undefined && /[a-z0-9]/i.test(character);
}
