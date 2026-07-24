import { LoaderCircle } from "lucide-react";

import type { BackendConnectionState } from "../../shared/state/BackendStateCache.ts";
import { TascarrelLogo } from "./TascarrelLogo.tsx";

export function ConnectionOverlay({
  connection,
  attempt = 1,
}: {
  connection: BackendConnectionState;
  attempt?: number;
}) {
  if (connection === "live") return null;

  const reconnecting = connection === "reconnecting";
  const title = reconnecting
    ? "Reconnecting to Tascarrel…"
    : "Connecting to Tascarrel…";

  return (
    <div
      className="fixed inset-0 z-[100] flex items-center justify-center bg-canvas text-foreground"
      role="status"
      aria-busy="true"
      aria-live="polite"
    >
      <section className="w-full max-w-sm px-8 text-center">
        <TascarrelLogo className="mx-auto size-14" />
        <h1 className="mt-5 text-lg font-semibold tracking-tight">{title}</h1>
        <div className="mt-4 flex items-center justify-center gap-2 text-[11px] text-muted">
          <LoaderCircle className="animate-spin text-accent" aria-hidden="true" size={14} />
          <span>Attempt {Math.max(1, attempt)}</span>
        </div>
      </section>
    </div>
  );
}
