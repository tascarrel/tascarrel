import { LoaderCircle } from "lucide-react";
import {
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
  type ReactNode,
  type UIEvent,
} from "react";

import type { images, pods, processes, workspaces } from "../../api/generated/index.ts";
import { LifecycleScreenFrame } from "../../components/ui/LifecycleScreenFrame.tsx";
import { useImageLog } from "../images/state.ts";
import { useProcessLog } from "../processes/state.ts";

export function PodStartupScreen({
  pod,
  workspace,
}: {
  pod: pods.Pod & { status: StartingPodState };
  workspace: workspaces.WorkspaceName;
}) {
  const presentation = startupPresentation(pod.status);
  return (
    <LifecycleScreenFrame
      icon={<LoaderCircle aria-hidden="true" className="size-4 animate-spin" />}
      log={<PodStartupLog state={pod.status} workspace={workspace} />}
      title={presentation.title}
    >
      <p aria-live="polite" className="mx-auto max-w-xl text-xs leading-5 text-muted">
        {presentation.detail}
      </p>
      {pod.status.status === "Creating" ? <PodCreationTiming state={pod.status} /> : null}
    </LifecycleScreenFrame>
  );
}

export function isPodStarting(
  pod: pods.Pod,
): pod is pods.Pod & { status: StartingPodState } {
  return pod.status.status === "Creating"
    || pod.status.status === "Building"
    || pod.status.status === "Starting"
    || pod.status.status === "Initializing";
}

type StartingPodState = Extract<
  pods.PodState,
  { status: "Creating" | "Building" | "Starting" | "Initializing" }
>;

type CreatingPodState = Extract<pods.PodState, { status: "Creating" }>;

function PodStartupLog({
  state,
  workspace,
}: {
  state: StartingPodState;
  workspace: workspaces.WorkspaceName;
}) {
  if (state.status === "Creating") return null;
  if (state.status === "Building") {
    return <ImageBuildLog imageId={state.imageId} workspace={workspace} />;
  }
  if (state.status === "Initializing") {
    return <InitializationLogs processes={state.processes} workspace={workspace} />;
  }
  return (
    <StartupLogFrame label="Pod startup log">
      <pre className={LOG_OUTPUT_CLASS_NAME} role="log" aria-label="Pod startup output">
        Starting the pod runtime. Waiting for initialization output…
      </pre>
    </StartupLogFrame>
  );
}

function PodCreationTiming({ state }: { state: CreatingPodState }) {
  const [now, setNow] = useState(Date.now);
  useEffect(() => {
    setNow(Date.now());
    const interval = window.setInterval(() => setNow(Date.now()), 1_000);
    return () => window.clearInterval(interval);
  }, [state.updatedAt]);
  const updatedAt = new Date(state.updatedAt);
  const updatedAtMillis = updatedAt.getTime();
  if (Number.isNaN(updatedAtMillis)) return null;
  return (
    <p className="mt-2 text-[10px] text-subtle">
      <time dateTime={state.updatedAt} title={updatedAt.toLocaleString()}>
        Current step for {formatElapsed(now - updatedAtMillis)}
      </time>
    </p>
  );
}

function ImageBuildLog({
  imageId,
  workspace,
}: {
  imageId: images.ImageId;
  workspace: workspaces.WorkspaceName;
}) {
  const logState = useImageLog(workspace, imageId);
  const logFollower = useFollowingLog(logState.value);
  return (
    <StartupLogFrame error={logState.error} label="Pod image build log">
      <pre
        ref={logFollower.output}
        className={LOG_OUTPUT_CLASS_NAME}
        role="log"
        aria-label="Pod image build output"
        onScroll={logFollower.onScroll}
      >
        {logState.value?.length
          ? logState.value.map(formatImageLogLine).join("\n")
          : logState.ready ? "No retained image build output." : "Loading image build output…"}
      </pre>
    </StartupLogFrame>
  );
}

function InitializationLogs({
  processes,
  workspace,
}: {
  processes: readonly pods.PodInitializationProcess[];
  workspace: workspaces.WorkspaceName;
}) {
  if (processes.length === 0) {
    return (
      <StartupLogFrame label="Pod initialization log">
        <pre className={LOG_OUTPUT_CLASS_NAME} role="log" aria-label="Pod initialization output">
          Waiting for initialization output…
        </pre>
      </StartupLogFrame>
    );
  }
  return (
    <div className="grid gap-3">
      {processes.map((process, index) => (
        <InitializationProcessLog
          index={index}
          key={process.processId}
          process={process}
          workspace={workspace}
        />
      ))}
    </div>
  );
}

function InitializationProcessLog({
  index,
  process,
  workspace,
}: {
  index: number;
  process: pods.PodInitializationProcess;
  workspace: workspaces.WorkspaceName;
}) {
  const logState = useProcessLog(workspace, process.processId);
  const logFollower = useFollowingLog(logState.value);
  const label = `Initialization step ${index + 1}${process.wait ? "" : " (background)"}`;
  return (
    <StartupLogFrame error={logState.error} label={label}>
      <pre
        ref={logFollower.output}
        className={LOG_OUTPUT_CLASS_NAME}
        role="log"
        aria-label={`${label} output`}
        onScroll={logFollower.onScroll}
      >
        {logState.value?.length
          ? logState.value.map(formatProcessLogLine).join("\n")
          : logState.ready ? "No retained initialization output." : "Loading initialization output…"}
      </pre>
    </StartupLogFrame>
  );
}

function StartupLogFrame({
  children,
  error,
  label,
}: {
  children: ReactNode;
  error?: Error;
  label: string;
}) {
  return (
    <section
      className="overflow-hidden rounded-lg border border-divider-strong bg-surface/30"
      aria-label={label}
    >
      <div className="border-b border-divider-strong px-4 py-2 text-[10px] text-subtle">
        {label}
      </div>
      {error ? (
        <p className="border-b border-red-500/20 px-4 py-2 text-xs text-red-200" role="alert">
          {error.message}
        </p>
      ) : null}
      {children}
    </section>
  );
}

/** Keeps new log output visible until the user intentionally scrolls upward. */
function useFollowingLog(logLines: readonly unknown[] | undefined) {
  const output = useRef<HTMLPreElement>(null);
  const followsOutput = useRef(true);
  useLayoutEffect(() => {
    const element = output.current;
    if (!element || !followsOutput.current) return;
    element.scrollTop = element.scrollHeight;
  }, [logLines]);
  return {
    output,
    onScroll: (event: UIEvent<HTMLPreElement>) => {
      const element = event.currentTarget;
      followsOutput.current = element.scrollHeight - element.scrollTop - element.clientHeight < 8;
    },
  };
}

function startupPresentation(state: StartingPodState): {
  title: string;
  detail: string;
} {
  if (state.status === "Building") {
    return {
      title: "Building Pod Image",
      detail: "Building the workspace image required by this pod.",
    };
  }
  if (state.status === "Starting") {
    return {
      title: "Starting Pod",
      detail: "Starting the isolated pod runtime.",
    };
  }
  if (state.status === "Initializing") {
    return {
      title: "Initializing Pod",
      detail: "Running the workspace initialization steps required before the pod is ready.",
    };
  }
  return {
    title: "Creating Pod",
    detail: state.message,
  };
}

function formatElapsed(milliseconds: number): string {
  const seconds = Math.max(0, Math.floor(milliseconds / 1_000));
  if (seconds < 60) return `${seconds}s`;
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes}m ${seconds % 60}s`;
  const hours = Math.floor(minutes / 60);
  return `${hours}h ${minutes % 60}m`;
}

function formatImageLogLine(line: images.ImageLogLine): string {
  const source = line.source.type === "BuildKit" ? "buildkit" : "setup";
  return `[${source}] ${line.content}${line.truncated ? " …" : ""}`;
}

function formatProcessLogLine(line: processes.ProcessLogLine): string {
  const source = line.source.type === "Stderr" ? "[stderr] " : "";
  return `${source}${line.content}${line.truncated ? " …" : ""}`;
}

const LOG_OUTPUT_CLASS_NAME = "m-0 min-h-40 max-h-[min(22rem,42vh)] overflow-x-hidden overflow-y-auto whitespace-pre-wrap [overflow-wrap:anywhere] bg-transparent p-4 font-mono text-[11px] leading-5 text-muted/80";
