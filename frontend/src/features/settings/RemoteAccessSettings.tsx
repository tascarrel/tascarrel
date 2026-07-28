import { Radio, Trash2 } from "lucide-react";
import { useEffect, useState } from "react";

import { hostApi } from "../../api/client.ts";
import type { auth } from "../../api/generated/index.ts";
import { Button } from "../../components/ui/Button.tsx";
import { ConfirmDialog } from "../../components/ui/ConfirmDialog.tsx";

export function RemoteAccessSettings() {
  const [sessions, setSessions] = useState<auth.BrowserSession[]>([]);
  const [currentSessionId, setCurrentSessionId] = useState<auth.BrowserSessionId>();
  const [ready, setReady] = useState(false);
  const [error, setError] = useState<string>();
  const [revokeTarget, setRevokeTarget] = useState<auth.BrowserSession>();
  const [revoking, setRevoking] = useState(false);

  useEffect(
    () =>
      hostApi.subscribe(
        "auth_BrowserSessionsChanged",
        {},
        {
          onEvent: (event) => {
            setSessions(event.sessions);
            setCurrentSessionId(event.currentSessionId);
            setReady(true);
            setError(undefined);
          },
          onError: (cause) => setError(cause.message),
        },
      ),
    [],
  );

  const revoke = async () => {
    if (!revokeTarget || revoking) return;
    setRevoking(true);
    setError(undefined);
    try {
      await hostApi.execute("auth_RevokeBrowserSession", {
        sessionId: revokeTarget.id,
      });
      const revokedCurrent = revokeTarget.id === currentSessionId;
      setRevokeTarget(undefined);
      if (revokedCurrent) window.location.reload();
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
      setRevokeTarget(undefined);
    } finally {
      setRevoking(false);
    }
  };

  return (
    <>
      <div className="max-w-4xl">
        <h2 className="text-sm font-semibold text-foreground">Remote Access</h2>
        <p className="mt-1 max-w-2xl text-xs leading-5 text-subtle">
          Browsers paired with this host remain authorized until they expire or you revoke them.
          Route access issued from a browser is revoked with its parent session.
        </p>
        <div className="mt-4 rounded-xl border border-ui-border bg-surface/60 px-4 py-3 text-xs text-muted">
          Pair another browser with <code className="text-foreground">tascarrelctl auth pair</code> on
          the host.
        </div>
        {error ? (
          <p
            className="mt-4 rounded-xl border border-red-500/20 bg-red-500/5 px-3 py-2 text-xs text-red-200"
            role="alert"
          >
            {error}
          </p>
        ) : null}
        <div className="mt-5 grid gap-3">
          {sessions.map((session) => {
            const current = session.id === currentSessionId;
            return (
              <section
                className="flex flex-col gap-4 rounded-xl border border-ui-border bg-surface/60 p-4 sm:flex-row sm:items-center sm:justify-between"
                key={session.id}
              >
                <div className="min-w-0">
                  <div className="flex flex-wrap items-center gap-2">
                    <h3 className="text-xs font-semibold text-foreground">{session.label}</h3>
                    {current ? (
                      <span className="rounded-full bg-accent-soft px-2 py-0.5 text-[10px] font-medium text-accent-text">
                        This browser
                      </span>
                    ) : null}
                    {session.activeConnections > 0 ? (
                      <span className="inline-flex items-center gap-1 text-[10px] text-emerald-300">
                        <Radio aria-hidden="true" className="size-3" />
                        Connected
                      </span>
                    ) : null}
                  </div>
                  <p className="mt-1 truncate text-[11px] text-subtle">{session.origin}</p>
                  <p className="mt-1 text-[10px] text-subtle">
                    Last seen {formatTimestamp(session.lastSeenAt)} · expires{" "}
                    {formatTimestamp(session.expiresAt)}
                  </p>
                </div>
                <Button
                  aria-label={`Revoke ${session.label}`}
                  className="shrink-0"
                  size="small"
                  variant="danger"
                  onClick={() => setRevokeTarget(session)}
                >
                  <Trash2 aria-hidden="true" className="size-3.5" />
                  Revoke
                </Button>
              </section>
            );
          })}
          {ready && sessions.length === 0 ? (
            <p className="text-xs text-subtle">No active browser sessions.</p>
          ) : null}
          {!ready && !error ? <p className="text-xs text-subtle">Loading browser sessions…</p> : null}
        </div>
      </div>
      <ConfirmDialog
        confirmLabel="Revoke session"
        description={
          revokeTarget?.id === currentSessionId
            ? "Revoke this browser session? You will need a new pairing key to reconnect."
            : `Revoke ${revokeTarget?.label ?? "this browser"} and all HTTP route access issued to it?`
        }
        destructive
        open={revokeTarget !== undefined}
        pending={revoking}
        title="Revoke Browser Session?"
        onOpenChange={(open) => {
          if (!open) setRevokeTarget(undefined);
        }}
        onConfirm={() => void revoke()}
      />
    </>
  );
}

function formatTimestamp(timestamp: string): string {
  const value = new Date(timestamp);
  if (Number.isNaN(value.getTime())) return timestamp;
  return value.toLocaleString();
}
