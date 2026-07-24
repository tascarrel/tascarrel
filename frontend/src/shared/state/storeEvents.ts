import type { store } from "../../api/generated/index.ts";

export function applyStoreEvent<T, M>(
  current: T | undefined,
  event: store.StoreEvent<T, M>,
  applyMutation: (current: T, mutation: M) => T,
): { value: T; cursor: store.Stamp } {
  if (event.type === "Snapshot") {
    return { value: event.value, cursor: event.stamp };
  }
  if (!current) throw new Error("Received a backend mutation before its snapshot");
  return {
    value: applyMutation(current, event.mutation),
    cursor: event.stamp,
  };
}
