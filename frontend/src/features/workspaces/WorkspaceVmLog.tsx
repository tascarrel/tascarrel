import { useLayoutEffect, useRef } from "react";

import type { guest } from "../../api/generated/index.ts";
import { useWorkspaceVmLog } from "./runtimeState.ts";

type WorkspaceVmLogPresentation = "default" | "screen";

export function WorkspaceVmLog({
  guestInstanceId,
  presentation = "default",
}: {
  guestInstanceId?: guest.GuestInstanceId;
  presentation?: WorkspaceVmLogPresentation;
}) {
  return guestInstanceId
    ? <SubscribedWorkspaceVmLog guestInstanceId={guestInstanceId} presentation={presentation} />
    : <WorkspaceVmLogUnavailable presentation={presentation} />;
}

function SubscribedWorkspaceVmLog({
  guestInstanceId,
  presentation,
}: {
  guestInstanceId: guest.GuestInstanceId;
  presentation: WorkspaceVmLogPresentation;
}) {
  const logState = useWorkspaceVmLog(guestInstanceId);
  const output = useRef<HTMLPreElement>(null);
  const followsOutput = useRef(true);

  useLayoutEffect(() => {
    const element = output.current;
    if (!element || !followsOutput.current) return;
    element.scrollTop = element.scrollHeight;
  }, [logState.value]);

  return (
    <section className={logFrameClassName(presentation)} aria-label="VM console log">
      {logState.error ? (
        <p className="border-b border-red-500/20 px-4 py-2 text-xs text-red-200" role="alert">
          {logState.error.message}
        </p>
      ) : null}
      <pre
        ref={output}
        className={logOutputClassName(presentation)}
        role="log"
        aria-label="Workspace VM console output"
        onScroll={(event) => {
          const element = event.currentTarget;
          followsOutput.current = element.scrollHeight - element.scrollTop - element.clientHeight < 8;
        }}
      >
        {logState.value?.length
          ? logState.value.map((line) => `${line.content}${line.truncated ? " …" : ""}`).join("\n")
          : logState.ready ? "No retained VM console output." : "Loading VM console output…"}
      </pre>
    </section>
  );
}

function WorkspaceVmLogUnavailable({
  presentation,
}: {
  presentation: WorkspaceVmLogPresentation;
}) {
  return (
    <section className={logFrameClassName(presentation)} aria-label="VM console log">
      <pre className={logOutputClassName(presentation)} role="log" aria-label="Workspace VM console output">
        No VM instance is available for console output.
      </pre>
    </section>
  );
}

function logFrameClassName(presentation: WorkspaceVmLogPresentation): string {
  return presentation === "screen"
    ? "overflow-hidden rounded-lg border border-divider-strong bg-surface/30"
    : "overflow-hidden rounded-xl border border-ui-border";
}

function logOutputClassName(presentation: WorkspaceVmLogPresentation): string {
  const base = "m-0 overflow-x-hidden overflow-y-auto whitespace-pre-wrap [overflow-wrap:anywhere] p-4 font-mono text-[11px] leading-5";
  return presentation === "screen"
    ? `${base} min-h-40 max-h-[min(22rem,42vh)] bg-transparent text-muted/80`
    : `${base} max-h-72 bg-canvas text-muted`;
}
