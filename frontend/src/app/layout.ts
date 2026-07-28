import { useSyncExternalStore } from "react";

const MOBILE_LAYOUT_QUERY = "(max-width: 767px), (hover: none) and (pointer: coarse) and (max-width: 1100px)";

export function useMobileLayout(): boolean {
  return useSyncExternalStore(subscribe, mobileLayoutSnapshot, () => false);
}

function subscribe(onChange: () => void): () => void {
  const query = window.matchMedia(MOBILE_LAYOUT_QUERY);
  query.addEventListener("change", onChange);
  return () => query.removeEventListener("change", onChange);
}

function mobileLayoutSnapshot(): boolean {
  return window.matchMedia(MOBILE_LAYOUT_QUERY).matches;
}
