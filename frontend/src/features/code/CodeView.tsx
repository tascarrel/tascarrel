import { Code2, LoaderCircle } from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import { guestApi } from "../../api/client.ts";
import type { pods, workspaces } from "../../api/generated/index.ts";
import { Button } from "../../components/ui/Button.tsx";
import type { IframeFrameSpec } from "../../components/ui/IframePool.tsx";
import { useHttpRouteTicket } from "../network/routeAccess.ts";
import { useCodeFrame } from "./CodeFramePool.tsx";
import { DEFAULT_CODE_FOLDER } from "./folders.ts";
import { useCodeSessions } from "./state.ts";

export function CodeView({
  workspace,
  pod,
  folder,
}: {
  workspace: workspaces.WorkspaceName;
  pod: pods.Pod;
  folder: string;
}) {
  const sessions = useCodeSessions(workspace);
  const [anchor, setAnchor] = useState<HTMLDivElement | null>(null);
  const [launching, setLaunching] = useState(false);
  const [actionError, setActionError] = useState<string>();
  const attemptedTarget = useRef<string | undefined>(undefined);
  const pendingTargets = useRef(new Set<string>());
  const session = sessions.value?.codeSessions.find((candidate) =>
    candidate.podId === pod.id && candidate.folder === folder
  );
  const routeAccess = useHttpRouteTicket(
    session?.status.status === "Running" ? session.hostnamePrefix : undefined,
  );
  const targetKey = `${workspace}:${pod.id}:${folder}`;
  const currentTarget = useRef(targetKey);

  useEffect(() => {
    currentTarget.current = targetKey;
  }, [targetKey]);

  const ensureSession = useCallback(async () => {
    if (pendingTargets.current.has(targetKey)) return;
    pendingTargets.current.add(targetKey);
    setLaunching(true);
    setActionError(undefined);
    try {
      await guestApi(workspace).execute("code_EnsureSession", {
        workspace,
        podId: pod.id,
        title: codeSessionTitle(pod.title, folder),
        folder,
      });
    } catch (cause) {
      if (currentTarget.current === targetKey) setActionError(errorMessage(cause));
    } finally {
      pendingTargets.current.delete(targetKey);
      setLaunching(pendingTargets.current.has(currentTarget.current));
    }
  }, [folder, pod.id, pod.title, targetKey, workspace]);

  useEffect(() => {
    if (attemptedTarget.current === targetKey) return;
    attemptedTarget.current = targetKey;
    void ensureSession();
  }, [ensureSession, targetKey]);

  const frame = useMemo<IframeFrameSpec | undefined>(() =>
    session?.status.status === "Running" && routeAccess.url ? {
      id: `${targetKey}:${session.processId}`,
      src: routeAccess.url,
      title: `${pod.title} code editor · ${folder}`,
      iframeProps: { allow: "clipboard-read; clipboard-write" },
    } : undefined,
  [folder, pod.title, routeAccess.url, session?.processId, session?.status.status, targetKey]);
  useCodeFrame(frame, anchor);

  const failure = actionError
    ?? routeAccess.error?.message
    ?? sessions.error?.message
    ?? (session?.status.status === "Failed" ? session.status.message : undefined);
  const exited = session?.status.status === "Exited";
  const waiting = launching
    || !sessions.ready
    || session === undefined
    || session.status.status === "Starting"
    || routeAccess.pending;

  return (
    <div ref={setAnchor} className="absolute inset-0 overflow-hidden bg-canvas">
      {!frame ? (
        <div className="flex h-full flex-col items-center justify-center gap-3 px-6 text-center">
          {waiting ? (
            <LoaderCircle className="size-5 animate-spin text-accent" aria-label="Starting code editor" />
          ) : (
            <Code2 className="size-5 text-subtle" aria-hidden="true" />
          )}
          <div>
            <strong className="block text-xs font-semibold text-muted">
              {waiting ? "Starting code editor" : exited ? "Code editor exited" : "Code editor unavailable"}
            </strong>
            <p className="mt-1.5 max-w-md text-[10px] leading-4 text-subtle">
              {failure ?? (waiting
                ? `Launching code-server for ${pod.title} and waiting for its health endpoint.`
                : "Restart code-server to reconnect this editor.")}
            </p>
          </div>
          {!waiting ? (
            <Button size="small" disabled={launching} onClick={() => void ensureSession()}>
              Restart
            </Button>
          ) : null}
        </div>
      ) : null}
    </div>
  );
}

function errorMessage(cause: unknown): string {
  return cause instanceof Error ? cause.message : String(cause);
}

function codeSessionTitle(podTitle: string, folder: string): string {
  return folder === DEFAULT_CODE_FOLDER
    ? `Code · ${podTitle}`
    : `Code · ${podTitle} · ${folder.slice(DEFAULT_CODE_FOLDER.length + 1) || folder}`;
}
