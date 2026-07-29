import {
  Ban,
  Check,
  CircleStop,
  LoaderCircle,
  Pause,
  Play,
  ShieldCheck,
} from "lucide-react";
import { useEffect, useMemo, useState } from "react";

import { hostApi } from "../../api/client.ts";
import type {
  host_operations,
  workspaces,
} from "../../api/generated/index.ts";
import { Badge, type BadgeTone } from "../../components/ui/Badge.tsx";
import { Button } from "../../components/ui/Button.tsx";
import { useHostOperations } from "./state.ts";

export function HostOperationsView({
  workspace,
}: {
  workspace: workspaces.WorkspaceName;
}) {
  const state = useHostOperations(workspace);
  const operations = state.value?.operations ?? [];
  const [selectedId, setSelectedId] = useState<host_operations.HostOperationId>();
  const [pendingAction, setPendingAction] = useState<string>();
  const [actionError, setActionError] = useState<string>();
  const selected = operations.find((operation) => operation.id === selectedId)
    ?? operations[0];

  useEffect(() => {
    if (!selectedId && operations[0]) setSelectedId(operations[0].id);
    if (selectedId && !operations.some((operation) => operation.id === selectedId)) {
      setSelectedId(operations[0]?.id);
    }
  }, [operations, selectedId]);

  const groups = useMemo(() => ({
    approval: operations.filter((operation) => operation.state.status === "AwaitingApproval"),
    active: operations.filter((operation) =>
      operation.state.status === "Preparing"
      || operation.state.status === "Starting"
      || operation.state.status === "Running"
    ),
    history: operations.filter((operation) => isTerminal(operation.state)),
  }), [operations]);

  const resolve = async (decision: host_operations.HostOperationDecision) => {
    if (!selected || pendingAction) return;
    setPendingAction(decision.tag);
    setActionError(undefined);
    try {
      await hostApi.execute("hostOperations_Resolve", {
        operationId: selected.id,
        decision,
      });
    } catch (cause) {
      setActionError(errorMessage(cause));
    } finally {
      setPendingAction(undefined);
    }
  };

  const cancel = async () => {
    if (!selected || pendingAction) return;
    setPendingAction("cancel");
    setActionError(undefined);
    try {
      await hostApi.execute("hostOperations_Cancel", {
        operationId: selected.id,
      });
    } catch (cause) {
      setActionError(errorMessage(cause));
    } finally {
      setPendingAction(undefined);
    }
  };

  const visibleError = actionError ?? state.error?.message;

  return (
    <div className="flex h-full min-h-0 flex-col overflow-hidden bg-canvas text-foreground">
      <header className="flex items-center justify-between border-b border-ui-border px-5 py-3">
        <h1 className="text-sm font-semibold">Host Operations</h1>
        {state.connection !== "live"
          ? <span className="text-xs text-subtle">Connecting…</span>
          : null}
      </header>

      {visibleError ? (
        <p className="border-b border-red-500/20 bg-red-500/5 px-6 py-2.5 text-xs text-red-200" role="alert">
          {visibleError}
        </p>
      ) : null}

      <div className="grid min-h-0 flex-1 grid-cols-[minmax(260px,0.34fr)_minmax(0,1fr)]">
        <aside className="min-h-0 overflow-auto border-r border-ui-border bg-surface/30 p-3">
          <div className="space-y-4">
            <OperationGroup
              title="Awaiting Approval"
              operations={groups.approval}
              selectedId={selected?.id}
              onSelect={setSelectedId}
            />
            <OperationGroup
              title="Running"
              operations={groups.active}
              selectedId={selected?.id}
              onSelect={setSelectedId}
            />
            <OperationGroup
              title="History"
              operations={groups.history}
              selectedId={selected?.id}
              onSelect={setSelectedId}
            />
          </div>
        </aside>

        <main className="min-h-0 overflow-auto">
          {selected ? (
            <OperationDetail
              operation={selected}
              pendingAction={pendingAction}
              onResolve={(decision) => void resolve(decision)}
              onCancel={() => void cancel()}
            />
          ) : (
            <div className="flex h-full items-center justify-center text-xs text-subtle">
              {state.ready ? "No operations" : "Loading…"}
            </div>
          )}
        </main>
      </div>
    </div>
  );
}

function OperationGroup({
  title,
  operations,
  selectedId,
  onSelect,
}: {
  title: string;
  operations: readonly host_operations.HostOperation[];
  selectedId?: host_operations.HostOperationId;
  onSelect: (id: host_operations.HostOperationId) => void;
}) {
  if (operations.length === 0) return null;

  return (
    <section>
      <div className="mb-1.5 flex items-center justify-between px-1">
        <h2 className="text-[10px] font-semibold uppercase tracking-[0.14em] text-subtle">
          {title}
        </h2>
        <span className="text-[10px] text-subtle">{operations.length}</span>
      </div>
      <div className="space-y-1">
        {operations.map((operation) => {
          const selected = selectedId === operation.id;
          return (
            <button
              aria-current={selected ? "true" : undefined}
              className="w-full rounded-lg border border-ui-border px-3 py-2 text-left outline-none transition hover:bg-surface-raised focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-accent data-[selected=true]:border-accent/45 data-[selected=true]:bg-accent/[0.07]"
              data-selected={selected}
              type="button"
              key={operation.id}
              onClick={() => onSelect(operation.id)}
            >
              <div className="flex items-center justify-between gap-2">
                <span className="truncate text-xs font-medium">{operation.command}</span>
                <StateBadge state={operation.state} />
              </div>
              <time
                className="mt-1 block text-[10px] text-subtle"
                dateTime={operation.createdAt}
              >
                {new Date(operation.createdAt).toLocaleString()}
              </time>
            </button>
          );
        })}
      </div>
    </section>
  );
}

function OperationDetail({
  operation,
  pendingAction,
  onResolve,
  onCancel,
}: {
  operation: host_operations.HostOperation;
  pendingAction?: string;
  onResolve: (decision: host_operations.HostOperationDecision) => void;
  onCancel: () => void;
}) {
  const awaiting = operation.state.status === "AwaitingApproval";
  const active = operation.state.status === "Preparing"
    || operation.state.status === "Starting"
    || operation.state.status === "Running";
  return (
    <div className="mx-auto max-w-5xl p-5">
      <div className="flex flex-wrap items-start justify-between gap-4">
        <div className="min-w-0">
          <div className="flex items-center gap-2">
            <h2 className="break-all text-lg font-semibold">{operation.command}</h2>
            <StateBadge state={operation.state} />
          </div>
          {operation.description
            ? <p className="mt-1 text-xs text-muted">{operation.description}</p>
            : null}
        </div>
        <div className="flex flex-wrap gap-2">
          {awaiting ? (
            <>
              <Button
                variant="primary"
                disabled={Boolean(pendingAction)}
                onClick={() => onResolve({ tag: "Approve" })}
              >
                {pendingAction === "Approve"
                  ? <LoaderCircle aria-hidden="true" className="animate-spin" size={13} />
                  : <Check aria-hidden="true" size={13} />}
                Approve
              </Button>
              <Button
                disabled={Boolean(pendingAction)}
                onClick={() => onResolve({ tag: "Postpone" })}
              >
                <Pause aria-hidden="true" size={13} /> Postpone
              </Button>
              <Button
                variant="danger"
                disabled={Boolean(pendingAction)}
                onClick={() => onResolve({ tag: "Reject" })}
              >
                <Ban aria-hidden="true" size={13} /> Reject
              </Button>
            </>
          ) : null}
          {active ? (
            <Button
              variant="danger"
              disabled={Boolean(pendingAction)}
              onClick={onCancel}
            >
              {pendingAction === "cancel"
                ? <LoaderCircle aria-hidden="true" className="animate-spin" size={13} />
                : <CircleStop aria-hidden="true" size={13} />}
              {operation.state.status === "Running" ? "Stop" : "Withdraw"}
            </Button>
          ) : null}
        </div>
      </div>

      <section className="mt-5 rounded-lg border border-ui-border bg-surface/50 p-3.5">
        <h3 className="text-xs font-semibold">Command</h3>
        <dl className="mt-3 grid grid-cols-[90px_minmax(0,1fr)] gap-x-4 gap-y-2 text-xs">
          <dt className="text-subtle">Program</dt>
          <dd className="break-all font-mono text-foreground">{operation.program}</dd>
          <dt className="text-subtle">Arguments</dt>
          <dd className="break-all font-mono text-foreground">
            {operation.arguments.length ? operation.arguments.join(" ") : "—"}
          </dd>
          <dt className="text-subtle">Working Directory</dt>
          <dd className="break-all font-mono text-foreground">
            {operation.workingDirectory ?? "—"}
          </dd>
          <dt className="text-subtle">Environment</dt>
          <dd className="font-mono text-foreground">
            {operation.environmentNames.length ? operation.environmentNames.join(", ") : "none"}
          </dd>
          <dt className="text-subtle">Pod</dt>
          <dd className="font-mono text-foreground">{operation.podId}</dd>
        </dl>
      </section>

      {operation.inputs.length ? (
        <section className="mt-3 rounded-lg border border-ui-border bg-surface/50 p-3.5">
          <h3 className="text-xs font-semibold">Repository Inputs</h3>
          <div className="mt-2 divide-y divide-ui-border/70">
            {operation.inputs.map((input) => (
              <div className="py-3 first:pt-1 last:pb-0" key={input.name}>
                <div className="flex flex-wrap items-center gap-2">
                  <span className="text-xs font-medium">{input.name}</span>
                  <Badge size="xs">{captureLabel(input.capture)}</Badge>
                  <span className="font-mono text-[10px] text-subtle">/workspace/{input.repository}</span>
                </div>
                <p className="mt-2 whitespace-pre-wrap font-mono text-[11px] leading-5 text-muted">
                  {input.changeSummary ?? (input.revision
                    ? input.revision
                    : "Waiting for input…")}
                </p>
              </div>
            ))}
          </div>
        </section>
      ) : null}

      <OperationStreams operation={operation} />
    </div>
  );
}

function OperationStreams({
  operation,
}: {
  operation: host_operations.HostOperation;
}) {
  const [chunks, setChunks] = useState<host_operations.HostOperationOutputChunk[]>([]);
  const [audit, setAudit] = useState<host_operations.HostOperationAuditEntry[]>([]);
  const [streamError, setStreamError] = useState<string>();
  const decodedChunks = useMemo(() => decodeOutputChunks(chunks), [chunks]);

  useEffect(() => {
    setChunks([]);
    setAudit([]);
    setStreamError(undefined);
    const stopOutput = hostApi.subscribe(
      "hostOperations_Output",
      { operationId: operation.id },
      {
        onEvent: (event) => {
          if (event.update.tag !== "Chunk") return;
          const chunk = event.update;
          setChunks((current) =>
            upsertSequence(current, chunk, (value) => String(value.sequence)));
        },
        onError: (error) => setStreamError(error.message),
      },
    );
    const stopAudit = hostApi.subscribe(
      "hostOperations_Audit",
      { operationId: operation.id },
      {
        onEvent: (event) => {
          if (event.update.tag !== "Event") return;
          const entry = event.update;
          setAudit((current) =>
            upsertSequence(current, entry, (value) => String(value.sequence)));
        },
        onError: (error) => setStreamError(error.message),
      },
    );
    return () => {
      stopOutput();
      stopAudit();
    };
  }, [operation.id]);

  return (
    <>
      <section className="mt-3 overflow-hidden rounded-lg border border-ui-border bg-[#090b10]">
        <div className="border-b border-ui-border px-3.5 py-2.5">
          <h3 className="text-xs font-semibold">Output</h3>
        </div>
        <pre className="min-h-24 max-h-96 overflow-auto whitespace-pre-wrap break-words p-3.5 font-mono text-[11px] leading-5 text-slate-200">
          {decodedChunks.length
            ? decodedChunks.map((chunk) => (
                <span
                  className={chunk.source === "Stderr" ? "text-red-300" : undefined}
                  key={chunk.sequence}
                >
                  {chunk.text}
                </span>
              ))
            : <span className="text-subtle">—</span>}
        </pre>
      </section>

      <section className="mt-3 rounded-lg border border-ui-border bg-surface/50 p-3.5">
        <h3 className="text-xs font-semibold">Audit Log</h3>
        {streamError ? <p className="mt-2 text-xs text-red-300">{streamError}</p> : null}
        <ol className="mt-3 space-y-2">
          {audit.map((entry) => (
            <li className="grid grid-cols-[150px_100px_minmax(0,1fr)] gap-3 text-[11px]" key={entry.sequence}>
              <time className="text-subtle">{new Date(entry.timestamp).toLocaleString()}</time>
              <span className="font-mono text-accent-text">{auditKindLabel(entry.kind)}</span>
              <span className="text-muted">{entry.message}</span>
            </li>
          ))}
          {audit.length === 0 ? <li className="text-xs text-subtle">—</li> : null}
        </ol>
      </section>
    </>
  );
}

function StateBadge({ state }: { state: host_operations.HostOperationState }) {
  const presentation = statePresentation(state);
  const Icon = presentation.icon;
  return (
    <Badge size="xs" tone={presentation.tone}>
      <Icon
        aria-hidden="true"
        className={presentation.spin ? "animate-spin" : undefined}
        size={10}
      />
      {presentation.label}
    </Badge>
  );
}

function statePresentation(state: host_operations.HostOperationState): {
  label: string;
  tone: BadgeTone;
  icon: typeof Play;
  spin?: boolean;
} {
  switch (state.status) {
    case "Preparing":
      return { label: "Preparing", tone: "muted", icon: LoaderCircle, spin: true };
    case "AwaitingApproval":
      return { label: state.postponed ? "Postponed" : "Approval", tone: "warning", icon: ShieldCheck };
    case "Starting":
      return { label: "Starting", tone: "primary", icon: LoaderCircle, spin: true };
    case "Running":
      return { label: "Running", tone: "primary", icon: Play };
    case "Succeeded":
      return { label: "Succeeded", tone: "success", icon: Check };
    case "Failed":
      return { label: "Failed", tone: "danger", icon: Ban };
    case "Rejected":
      return { label: "Rejected", tone: "danger", icon: Ban };
    case "Canceled":
      return { label: "Canceled", tone: "muted", icon: CircleStop };
    case "Interrupted":
      return { label: "Interrupted", tone: "warning", icon: CircleStop };
  }
}

function isTerminal(state: host_operations.HostOperationState) {
  return state.status === "Succeeded"
    || state.status === "Failed"
    || state.status === "Rejected"
    || state.status === "Canceled"
    || state.status === "Interrupted";
}

function captureLabel(capture: host_operations.HostOperationCapture): string {
  switch (capture.tag) {
    case "WorkingTree":
      return "working tree";
    case "CleanHead":
      return "clean HEAD";
    case "Commit":
      return "commit";
    case "PublishedRef":
      return "published ref";
  }
}

function auditKindLabel(kind: host_operations.HostOperationAuditKind): string {
  switch (kind) {
    case "Requested":
      return "requested";
    case "InputCaptured":
      return "input captured";
    case "Postponed":
      return "postponed";
    case "Approved":
      return "approved";
    case "Rejected":
      return "rejected";
    case "Started":
      return "started";
    case "CancelRequested":
      return "cancel requested";
    case "Canceled":
      return "canceled";
    case "TimedOut":
      return "timed out";
    case "Succeeded":
      return "succeeded";
    case "Failed":
      return "failed";
    case "Interrupted":
      return "interrupted";
  }
}

function upsertSequence<T>(
  values: readonly T[],
  value: T,
  sequence: (value: T) => string,
): T[] {
  if (values.some((existing) => sequence(existing) === sequence(value))) return [...values];
  return [...values, value].toSorted((left, right) => {
    const leftSequence = BigInt(sequence(left));
    const rightSequence = BigInt(sequence(right));
    return leftSequence < rightSequence ? -1 : leftSequence > rightSequence ? 1 : 0;
  });
}

function decodeBase64(value: string): Uint8Array {
  const binary = atob(value);
  return Uint8Array.from(binary, (character) => character.charCodeAt(0));
}

function decodeOutputChunks(
  chunks: readonly host_operations.HostOperationOutputChunk[],
): {
  sequence: host_operations.HostOperationOutputChunk["sequence"];
  source: "Stdout" | "Stderr";
  text: string;
}[] {
  const decoders = {
    Stdout: new TextDecoder(),
    Stderr: new TextDecoder(),
  };
  const decoded = chunks.map((chunk) => ({
    sequence: chunk.sequence,
    source: chunk.source.tag,
    text: decoders[chunk.source.tag].decode(decodeBase64(chunk.data), { stream: true }),
  }));
  const stdoutRemainder = decoders.Stdout.decode();
  const stderrRemainder = decoders.Stderr.decode();
  if (stdoutRemainder) {
    const last = decoded.findLast((chunk) => chunk.source === "Stdout");
    if (last) last.text += stdoutRemainder;
  }
  if (stderrRemainder) {
    const last = decoded.findLast((chunk) => chunk.source === "Stderr");
    if (last) last.text += stderrRemainder;
  }
  return decoded;
}

function errorMessage(cause: unknown): string {
  return cause instanceof Error ? cause.message : String(cause);
}
