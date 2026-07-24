import { Copy, FileText, LoaderCircle, Play, Send, Trash2 } from "lucide-react";
import { useEffect, useState, type FormEvent } from "react";

import { guestApi } from "../../api/client.ts";
import type { pods, processes, workspaces } from "../../api/generated/index.ts";
import { Badge } from "../../components/ui/Badge.tsx";
import { Button } from "../../components/ui/Button.tsx";
import { SelectControl } from "../../components/ui/SelectControl.tsx";
import { ProcessTerminal } from "./ProcessTerminal.tsx";
import { useProcessLog, useProcesses } from "./state.ts";

export function ProcessManager({
  workspace,
  pod,
}: {
  workspace: workspaces.WorkspaceName;
  pod: pods.Pod;
}) {
  const processState = useProcesses(workspace);
  const podProcesses = (processState.value?.processes ?? []).filter((process) => process.podId === pod.id);
  const [selectedId, setSelectedId] = useState<processes.ProcessId>();
  const [creating, setCreating] = useState(false);
  const [actionError, setActionError] = useState<string>();

  useEffect(() => {
    if (selectedId && podProcesses.some((process) => process.id === selectedId)) return;
    setSelectedId(podProcesses.at(-1)?.id);
  }, [podProcesses, selectedId]);

  const selectedProcess = podProcesses.find((process) => process.id === selectedId);
  return (
    <section className="overflow-hidden rounded-xl border border-ui-border bg-surface/30" aria-labelledby="process-manager-title">
      <header className="flex flex-wrap items-center justify-between gap-3 border-b border-ui-border px-4 py-3">
        <div>
          <h2 className="text-xs font-semibold text-foreground" id="process-manager-title">Supervised Processes</h2>
          <p className="mt-1 text-[11px] text-subtle">Run commands and inspect retained output for this pod.</p>
        </div>
        <div className="flex gap-2">
          <Button size="small" variant="primary" onClick={() => setCreating((current) => !current)}>
            <Play aria-hidden="true" className="size-3.5" /> {creating ? "Cancel" : "Run process"}
          </Button>
        </div>
      </header>
      {actionError || processState.error ? (
        <p className="border-b border-red-500/20 bg-red-500/5 px-4 py-2 text-xs text-red-200" role="alert">
          {actionError ?? processState.error?.message}
        </p>
      ) : null}
      {creating ? (
        <SpawnProcessForm
          workspace={workspace}
          podId={pod.id}
          onCreated={(processId) => {
            setSelectedId(processId);
            setCreating(false);
          }}
          onError={(cause) => setActionError(errorMessage(cause))}
        />
      ) : null}
      <div className="grid min-h-72 md:grid-cols-[16rem_minmax(0,1fr)]">
        <div className="max-h-[32rem] overflow-y-auto border-b border-ui-border p-2 md:border-r md:border-b-0">
          {podProcesses.map((process) => (
            <button
              className="mb-1 flex w-full items-center gap-2 rounded-lg px-2.5 py-2 text-left text-xs text-muted hover:bg-surface-raised data-[selected=true]:bg-surface-raised data-[selected=true]:text-foreground"
              type="button"
              data-selected={process.id === selectedId}
              aria-pressed={process.id === selectedId}
              key={process.id}
              onClick={() => setSelectedId(process.id)}
            >
              <span className="min-w-0 flex-1">
                <span className="block truncate font-medium">{process.title || "Untitled process"}</span>
                <span className="mt-1 block truncate font-mono text-[9px] text-subtle">{shortId(process.id)}</span>
              </span>
              <ProcessStatusBadge status={process.status} />
            </button>
          ))}
          {podProcesses.length === 0 ? (
            <p className="p-3 text-xs leading-5 text-subtle">
              {processState.ready ? "No retained processes for this pod." : "Loading processes…"}
            </p>
          ) : null}
        </div>
        {selectedProcess ? (
          <ProcessDetails
            workspace={workspace}
            process={selectedProcess}
            onError={(cause) => setActionError(errorMessage(cause))}
          />
        ) : (
          <div className="flex items-center justify-center p-6 text-center text-xs text-subtle">
            Select a process to inspect its output.
          </div>
        )}
      </div>
    </section>
  );
}

function SpawnProcessForm({
  workspace,
  podId,
  onCreated,
  onError,
}: {
  workspace: workspaces.WorkspaceName;
  podId: pods.PodId;
  onCreated: (processId: processes.ProcessId) => void;
  onError: (cause: unknown) => void;
}) {
  const [title, setTitle] = useState("Shell command");
  const [executable, setExecutable] = useState("bash");
  const [argumentsText, setArgumentsText] = useState("-lc\necho 'Hello from Tascarrel'");
  const [environmentText, setEnvironmentText] = useState("");
  const [workingDirectory, setWorkingDirectory] = useState("");
  const [profile, setProfile] = useState<"User" | "SystemService">("User");
  const [terminal, setTerminal] = useState(false);
  const [startPod, setStartPod] = useState(true);
  const [logStdout, setLogStdout] = useState(true);
  const [pending, setPending] = useState(false);

  const submit = async (event: FormEvent) => {
    event.preventDefault();
    if (pending || !title.trim() || !executable.trim()) return;
    setPending(true);
    try {
      const output = await guestApi(workspace).execute("processes_Spawn", {
        podId,
        title: title.trim(),
        executable: executable.trim(),
        arguments: nonEmptyLines(argumentsText),
        environment: parseEnvironment(environmentText),
        ...(workingDirectory.trim() ? { workingDirectory: workingDirectory.trim() } : {}),
        ...(terminal ? { terminal: DEFAULT_TERMINAL_SIZE } : {}),
        startPod,
        logStdout,
        profile: { type: profile },
      });
      onCreated(output.processId);
    } catch (cause) {
      onError(cause);
    } finally {
      setPending(false);
    }
  };

  return (
    <form className="grid gap-3 border-b border-ui-border bg-canvas/50 p-4" onSubmit={(event) => void submit(event)}>
      <div className="grid gap-3 sm:grid-cols-2">
        <TextField label="Title" value={title} onChange={setTitle} />
        <TextField label="Executable" value={executable} onChange={setExecutable} />
        <TextField label="Working directory (optional)" value={workingDirectory} onChange={setWorkingDirectory} />
        <SelectControl
          label="Execution profile"
          value={profile}
          options={[{ label: "Pod user", value: "User" }, { label: "System service", value: "SystemService" }]}
          onChange={(value) => setProfile(value as "User" | "SystemService")}
        />
      </div>
      <div className="grid gap-3 sm:grid-cols-2">
        <TextAreaField label="Arguments (one per line)" value={argumentsText} onChange={setArgumentsText} />
        <TextAreaField label="Environment (KEY=value per line)" value={environmentText} onChange={setEnvironmentText} />
      </div>
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div className="flex flex-wrap gap-4 text-[11px] text-muted">
          <Checkbox label="Start pod if needed" checked={startPod} onChange={setStartPod} />
          <Checkbox label="Retain stdout" checked={logStdout} onChange={setLogStdout} />
          <Checkbox label="Allocate PTY" checked={terminal} onChange={setTerminal} />
        </div>
        <Button type="submit" size="small" variant="primary" disabled={pending || !title.trim() || !executable.trim()}>
          {pending ? <LoaderCircle aria-hidden="true" className="size-3.5 animate-spin" /> : <Play aria-hidden="true" className="size-3.5" />}
          {pending ? "Starting…" : "Start process"}
        </Button>
      </div>
    </form>
  );
}

function ProcessDetails({
  workspace,
  process,
  onError,
}: {
  workspace: workspaces.WorkspaceName;
  process: processes.Process;
  onError: (cause: unknown) => void;
}) {
  const [signal, setSignal] = useState<processes.ProcessSignal["type"]>("Terminate");
  const [pending, setPending] = useState(false);
  const [snapshot, setSnapshot] = useState<string>();
  const running = process.status.status === "Starting" || process.status.status === "Running";
  const removable = process.status.status === "Exited" || process.status.status === "Failed";
  const run = async (operation: () => Promise<void>) => {
    setPending(true);
    try {
      await operation();
    } catch (cause) {
      onError(cause);
    } finally {
      setPending(false);
    }
  };

  return (
    <div className="flex min-h-0 flex-col overflow-hidden">
      <div className="border-b border-ui-border px-4 py-3">
        <div className="flex flex-wrap items-start justify-between gap-3">
          <div className="min-w-0">
            <h3 className="truncate text-xs font-semibold text-foreground">{process.title}</h3>
            <p className="mt-1 text-[10px] text-subtle">Started by {actorLabel(process.startedBy)} · {formatTimestamp(process.createdAt)}</p>
          </div>
          <div className="flex flex-wrap items-end gap-2">
            {running ? (
              <>
                <SelectControl
                  className="w-28"
                  label="Signal"
                  value={signal}
                  options={SIGNALS.map((value) => ({ label: value, value }))}
                  hideLabel
                  onChange={(value) => setSignal(value as processes.ProcessSignal["type"])}
                />
                <Button size="small" disabled={pending} onClick={() => void run(async () => {
                  await guestApi(workspace).execute("processes_Kill", { processId: process.id, signal: { type: signal } });
                })}>
                  <Send aria-hidden="true" className="size-3.5" /> Send signal
                </Button>
              </>
            ) : null}
            {process.terminal ? (
              <Button size="small" disabled={pending} onClick={() => void run(async () => {
                const output = await guestApi(workspace).execute("processes_SnapshotTerminal", { processId: process.id });
                setSnapshot(output.snapshot.replaceAll("\u001b", "␛"));
              })}>
                <Copy aria-hidden="true" className="size-3.5" /> Snapshot
              </Button>
            ) : null}
            {removable ? (
              <Button size="small" variant="danger" disabled={pending} onClick={() => void run(async () => {
                await guestApi(workspace).execute("processes_Remove", { processId: process.id });
              })}>
                <Trash2 aria-hidden="true" className="size-3.5" /> Remove
              </Button>
            ) : null}
          </div>
        </div>
        {process.status.status === "Failed" ? <p className="mt-2 text-xs text-red-200">{process.status.message}</p> : null}
        {snapshot !== undefined ? (
          <details className="mt-3 rounded-lg border border-ui-border bg-canvas/60 p-2">
            <summary className="cursor-pointer text-[11px] text-muted">ANSI terminal snapshot</summary>
            <pre className="mt-2 max-h-48 overflow-auto whitespace-pre-wrap font-mono text-[10px] text-subtle">{snapshot || "Empty terminal snapshot."}</pre>
          </details>
        ) : null}
      </div>
      {process.terminal ? (
        <ProcessTerminal workspace={workspace} process={process} />
      ) : (
        <ProcessLog workspace={workspace} process={process} />
      )}
    </div>
  );
}

function ProcessLog({
  workspace,
  process,
}: {
  workspace: workspaces.WorkspaceName;
  process: processes.Process;
}) {
  const logState = useProcessLog(workspace, process.id);
  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <div className="flex items-center justify-between border-b border-ui-border px-4 py-2 text-[10px] text-subtle">
        <span className="flex items-center gap-1.5"><FileText aria-hidden="true" className="size-3" /> Process log</span>
        <span>{logState.connection}</span>
      </div>
      <pre className="m-0 min-h-52 flex-1 overflow-auto bg-canvas p-4 font-mono text-[11px] leading-5 text-muted" role="log" aria-label={`Process log for ${process.title}`}>
        {logState.value?.length
          ? logState.value.map((line) => `${line.source.type === "Stderr" ? "[stderr] " : ""}${line.content}${line.truncated ? " …" : ""}`).join("\n")
          : logState.ready ? "No retained process output." : "Loading process output…"}
      </pre>
    </div>
  );
}

function TextField({ label, value, onChange }: { label: string; value: string; onChange: (value: string) => void }) {
  return <label className="grid gap-1 text-[10px] text-subtle">{label}<input className="h-9 rounded-lg border border-ui-border bg-surface px-2.5 text-xs text-foreground outline-none focus:border-accent/50" value={value} onChange={(event) => onChange(event.target.value)} /></label>;
}

function TextAreaField({ label, value, onChange }: { label: string; value: string; onChange: (value: string) => void }) {
  return <label className="grid gap-1 text-[10px] text-subtle">{label}<textarea className="min-h-20 resize-y rounded-lg border border-ui-border bg-surface px-2.5 py-2 font-mono text-[11px] text-foreground outline-none focus:border-accent/50" value={value} onChange={(event) => onChange(event.target.value)} /></label>;
}

function Checkbox({ label, checked, onChange }: { label: string; checked: boolean; onChange: (checked: boolean) => void }) {
  return <label className="flex items-center gap-2"><input type="checkbox" checked={checked} onChange={(event) => onChange(event.target.checked)} /> {label}</label>;
}

function ProcessStatusBadge({ status }: { status: processes.ProcessState }) {
  const tone = status.status === "Failed" ? "danger" : status.status === "Running" ? "success" : status.status === "Exited" ? "muted" : "primary";
  return <Badge size="xs" tone={tone}>{status.status}</Badge>;
}

function parseEnvironment(value: string): Record<string, string> {
  return Object.fromEntries(nonEmptyLines(value).map((line) => {
    const separator = line.indexOf("=");
    return separator < 1 ? [line, ""] : [line.slice(0, separator), line.slice(separator + 1)];
  }));
}

function nonEmptyLines(value: string): string[] {
  return value.split("\n").map((line) => line.trim()).filter(Boolean);
}

function actorLabel(actor: processes.Process["startedBy"]): string {
  if (actor.type === "Client") return `client ${shortId(actor.clientId)}`;
  if (actor.type === "Host") return "host";
  if (actor.type === "Workspace") return actor.workspace;
  return `pod ${shortId(actor.podId)}`;
}

function shortId(value: string): string {
  return value.length > 20 ? `${value.slice(0, 9)}…${value.slice(-7)}` : value;
}

function formatTimestamp(value: string): string {
  const date = new Date(value);
  return Number.isNaN(date.getTime()) ? value : date.toLocaleString();
}

function errorMessage(cause: unknown): string {
  return cause instanceof Error ? cause.message : String(cause);
}

const SIGNALS: processes.ProcessSignal["type"][] = ["Terminate", "Interrupt", "Hangup", "Kill"];
const DEFAULT_TERMINAL_SIZE: processes.ProcessTerminal = {
  rows: 24 as processes.ProcessTerminal["rows"],
  cols: 80 as processes.ProcessTerminal["cols"],
};
