import {
  createContext,
  type ReactNode,
  useContext,
  useSyncExternalStore,
} from "react";

import {
  BackendStateCache,
  type BackendStateDefinition,
  type BackendStateResource,
  type BackendStateSnapshot,
} from "./BackendStateCache.ts";

const StateCacheContext = createContext<BackendStateCache | undefined>(undefined);

export function StateCacheProvider({
  cache,
  children,
}: {
  cache: BackendStateCache;
  children: ReactNode;
}) {
  return <StateCacheContext.Provider value={cache}>{children}</StateCacheContext.Provider>;
}

export function useBackendStateCache(): BackendStateCache {
  const cache = useContext(StateCacheContext);
  if (!cache) throw new Error("Backend state cache provider is missing");
  return cache;
}

export function useBackendState<T, E, C>(
  definition: BackendStateDefinition<T, E, C>,
): BackendStateSnapshot<T> & Pick<BackendStateResource<T>, "refresh" | "updateValue"> {
  const resource = useBackendStateCache().resource(definition);
  const snapshot = useSyncExternalStore(
    resource.subscribe,
    resource.getSnapshot,
    resource.getSnapshot,
  );
  return {
    ...snapshot,
    refresh: resource.refresh,
    updateValue: resource.updateValue,
  };
}
