import { useNavigate } from "@tanstack/react-router";
import {
  Bot,
  Check,
  CircleStop,
  Clock3,
  LoaderCircle,
  Play,
  ShieldCheck,
  X,
} from "lucide-react";
import { useEffect, useMemo, useState } from "react";

import { hostApi } from "../../api/client.ts";
import type {
  automations,
  protocol,
  workspaces,
} from "../../api/generated/index.ts";
import { Badge, type BadgeTone } from "../../components/ui/Badge.tsx";
import { Button } from "../../components/ui/Button.tsx";
import { useAutomationCatalog, useAutomationExecutions } from "./state.ts";

export function AutomationsView({
  workspace,
}: {
  workspace: workspaces.WorkspaceName;
}) {
  const navigate = useNavigate();
  const catalogState = useAutomationCatalog(workspace);
  const executionState = useAutomationExecutions(workspace);
  const definitions = catalogState.value?.automations ?? [];
  const executions = executionState.value?.executions ?? [];
  const [selection, setSelection] = useState<Selection>();
  const [pendingExecutionId, setPendingExecutionId] =
    useState<automations.AutomationExecutionId>();
  const [pendingAction, setPendingAction] = useState<string>();
  const [actionError, setActionError] = useState<string>();

  const selectedExecution =
    selection?.type === "execution"
      ? executions.find((execution) => execution.id === selection.id)
      : undefined;
  const selectedDefinition =
    selection?.type === "definition"
      ? definitions.find((definition) => definition.id === selection.id)
      : selectedExecution
        ? (definitions.find(
            (definition) => definition.id === selectedExecution.automationId,
          ) ?? selectedExecution.definition)
        : undefined;
  const runnableDefinition =
    selectedDefinition &&
    definitions.some((definition) => definition.id === selectedDefinition.id)
      ? selectedDefinition
      : undefined;

  useEffect(() => {
    const valid =
      selection?.type === "execution"
        ? executions.some((execution) => execution.id === selection.id)
        : selection?.type === "definition"
          ? definitions.some((definition) => definition.id === selection.id)
          : false;
    if (valid) {
      if (
        selection?.type === "execution" &&
        pendingExecutionId === selection.id
      ) {
        setPendingExecutionId(undefined);
      }
      return;
    }
    if (
      selection?.type === "execution" &&
      pendingExecutionId === selection.id
    ) {
      return;
    }
    if (executions[0]) {
      setSelection({ type: "execution", id: executions[0].id });
    } else if (definitions[0]) {
      setSelection({ type: "definition", id: definitions[0].id });
    } else {
      setSelection(undefined);
    }
  }, [definitions, executions, pendingExecutionId, selection]);

  const groups = useMemo(
    () => ({
      attention: executions.filter(
        (execution) =>
          execution.state === "WaitingForApproval" ||
          execution.state === "WaitingForInput",
      ),
      active: executions.filter(
        (execution) =>
          execution.state === "Queued" || execution.state === "Running",
      ),
      history: executions.filter((execution) => isTerminal(execution.state)),
    }),
    [executions],
  );

  const run = async (definition: automations.AutomationDefinition) => {
    if (pendingAction) return;
    setPendingAction(`run:${definition.id}`);
    setActionError(undefined);
    try {
      const output = await hostApi.execute("automations_Start", {
        workspace,
        automationId: definition.id,
      });
      setPendingExecutionId(output.executionId);
      setSelection({ type: "execution", id: output.executionId });
    } catch (cause) {
      setActionError(errorMessage(cause));
    } finally {
      setPendingAction(undefined);
    }
  };

  const cancel = async (execution: automations.AutomationExecution) => {
    if (pendingAction) return;
    setPendingAction("cancel");
    setActionError(undefined);
    try {
      await hostApi.execute("automations_Cancel", {
        executionId: execution.id,
      });
    } catch (cause) {
      setActionError(errorMessage(cause));
    } finally {
      setPendingAction(undefined);
    }
  };

  const resolve = async (
    execution: automations.AutomationExecution,
    decision: automations.AutomationApprovalDecision,
  ) => {
    if (pendingAction) return;
    setPendingAction(decision);
    setActionError(undefined);
    try {
      await hostApi.execute("automations_ResolveApproval", {
        executionId: execution.id,
        decision,
      });
    } catch (cause) {
      setActionError(errorMessage(cause));
    } finally {
      setPendingAction(undefined);
    }
  };

  const visibleError =
    actionError ?? catalogState.error?.message ?? executionState.error?.message;

  return (
    <div className="flex h-full min-h-0 flex-col overflow-hidden bg-canvas text-foreground">
      <header className="flex flex-wrap items-center justify-between gap-3 border-b border-ui-border px-5 py-3">
        <div>
          <h1 className="text-sm font-semibold">Automations</h1>
          <p className="mt-0.5 text-[11px] text-subtle">
            Durable, host-owned workflows from <code>automations/*.yaml</code>
          </p>
        </div>
        {runnableDefinition && hasManualTrigger(runnableDefinition) ? (
          <Button
            variant="primary"
            disabled={Boolean(pendingAction)}
            onClick={() => void run(runnableDefinition)}
          >
            {pendingAction === `run:${runnableDefinition.id}` ? (
              <LoaderCircle
                aria-hidden="true"
                className="animate-spin"
                size={13}
              />
            ) : (
              <Play aria-hidden="true" size={13} />
            )}
            Run workflow
          </Button>
        ) : null}
      </header>

      {visibleError ? (
        <p
          className="border-b border-red-500/20 bg-red-500/5 px-5 py-2.5 text-xs text-red-200"
          role="alert"
        >
          {visibleError}
        </p>
      ) : null}
      {(catalogState.value?.errors ?? []).length > 0 ? (
        <div
          className="border-b border-amber-500/20 bg-amber-500/5 px-5 py-2.5"
          role="alert"
        >
          {(catalogState.value?.errors ?? []).map((error) => (
            <p className="text-xs text-amber-200" key={error.path}>
              <span className="font-mono">{error.path}</span>: {error.message}
            </p>
          ))}
        </div>
      ) : null}

      <div className="grid min-h-0 flex-1 grid-cols-1 md:grid-cols-[minmax(270px,0.34fr)_minmax(0,1fr)]">
        <aside className="min-h-0 overflow-auto border-b border-ui-border bg-surface/30 p-3 md:border-b-0 md:border-r">
          <CatalogList
            definitions={definitions}
            selected={selection}
            onSelect={(id) => setSelection({ type: "definition", id })}
          />
          <div className="mt-5 space-y-4">
            <ExecutionGroup
              title="Needs Attention"
              executions={groups.attention}
              selected={selection}
              onSelect={(id) => setSelection({ type: "execution", id })}
            />
            <ExecutionGroup
              title="Running"
              executions={groups.active}
              selected={selection}
              onSelect={(id) => setSelection({ type: "execution", id })}
            />
            <ExecutionGroup
              title="History"
              executions={groups.history}
              selected={selection}
              onSelect={(id) => setSelection({ type: "execution", id })}
            />
          </div>
        </aside>

        <main className="min-h-0 overflow-auto">
          {selectedExecution ? (
            <ExecutionDetail
              execution={selectedExecution}
              pendingAction={pendingAction}
              onCancel={() => void cancel(selectedExecution)}
              onResolve={(decision) =>
                void resolve(selectedExecution, decision)
              }
              onOpenChat={() => {
                if (!selectedExecution.podId || !selectedExecution.chatId)
                  return;
                void navigate({
                  to: "/workspaces/$workspace/pods/$pod/chats/$chat",
                  params: {
                    workspace,
                    pod: selectedExecution.podId,
                    chat: selectedExecution.chatId,
                  },
                }).catch((cause) => setActionError(errorMessage(cause)));
              }}
            />
          ) : selectedDefinition ? (
            <DefinitionDetail definition={selectedDefinition} />
          ) : (
            <div className="flex h-full items-center justify-center p-8 text-center text-xs text-subtle">
              {catalogState.ready && executionState.ready
                ? "Add an Automation YAML file to this workspace configuration."
                : "Loading Automations…"}
            </div>
          )}
        </main>
      </div>
    </div>
  );
}

type Selection =
  | { type: "definition"; id: string }
  | { type: "execution"; id: automations.AutomationExecutionId };

const MAX_VISIBLE_OUTPUT_LINES = 5_000;

function CatalogList({
  definitions,
  selected,
  onSelect,
}: {
  definitions: readonly automations.AutomationDefinition[];
  selected?: Selection;
  onSelect: (id: string) => void;
}) {
  return (
    <section>
      <ListHeading title="Workflows" count={definitions.length} />
      {definitions.length ? (
        <div className="space-y-1">
          {definitions.map((definition) => {
            const active =
              selected?.type === "definition" && selected.id === definition.id;
            return (
              <button
                aria-current={active ? "true" : undefined}
                className="w-full rounded-lg border border-ui-border px-3 py-2 text-left outline-none transition hover:bg-surface-raised focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-accent data-[selected=true]:border-accent/45 data-[selected=true]:bg-accent/[0.07]"
                data-selected={active}
                key={definition.id}
                type="button"
                onClick={() => onSelect(definition.id)}
              >
                <div className="flex items-center gap-2">
                  <span className="truncate text-xs font-medium">
                    {definition.name}
                  </span>
                </div>
                <span className="mt-1 block truncate font-mono text-[10px] text-subtle">
                  {definition.id}
                </span>
              </button>
            );
          })}
        </div>
      ) : (
        <p className="px-1 py-2 text-xs text-subtle">No valid workflows</p>
      )}
    </section>
  );
}

function ExecutionGroup({
  title,
  executions,
  selected,
  onSelect,
}: {
  title: string;
  executions: readonly automations.AutomationExecution[];
  selected?: Selection;
  onSelect: (id: automations.AutomationExecutionId) => void;
}) {
  if (!executions.length) return null;
  return (
    <section>
      <ListHeading title={title} count={executions.length} />
      <div className="space-y-1">
        {executions.map((execution) => {
          const active =
            selected?.type === "execution" && selected.id === execution.id;
          return (
            <button
              aria-current={active ? "true" : undefined}
              className="w-full rounded-lg border border-ui-border px-3 py-2 text-left outline-none transition hover:bg-surface-raised focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-accent data-[selected=true]:border-accent/45 data-[selected=true]:bg-accent/[0.07]"
              data-selected={active}
              key={execution.id}
              type="button"
              onClick={() => onSelect(execution.id)}
            >
              <div className="flex items-center justify-between gap-2">
                <span className="truncate text-xs font-medium">
                  {execution.definition.name}
                </span>
                <ExecutionStateBadge state={execution.state} />
              </div>
              <time
                className="mt-1 block text-[10px] text-subtle"
                dateTime={execution.createdAt}
              >
                {formatTimestamp(execution.createdAt)}
              </time>
            </button>
          );
        })}
      </div>
    </section>
  );
}

function ListHeading({ title, count }: { title: string; count: number }) {
  return (
    <div className="mb-1.5 flex items-center justify-between px-1">
      <h2 className="text-[10px] font-semibold uppercase tracking-[0.14em] text-subtle">
        {title}
      </h2>
      <span className="text-[10px] text-subtle">{count}</span>
    </div>
  );
}

function DefinitionDetail({
  definition,
}: {
  definition: automations.AutomationDefinition;
}) {
  return (
    <div className="mx-auto max-w-5xl p-5">
      <h2 className="text-lg font-semibold">{definition.name}</h2>
      {definition.description ? (
        <p className="mt-2 max-w-3xl text-xs leading-5 text-muted">
          {definition.description}
        </p>
      ) : null}
      <DefinitionMetadata definition={definition} />
      <StepDefinitions steps={definition.steps} />
    </div>
  );
}

function ExecutionDetail({
  execution,
  pendingAction,
  onResolve,
  onCancel,
  onOpenChat,
}: {
  execution: automations.AutomationExecution;
  pendingAction?: string;
  onResolve: (decision: automations.AutomationApprovalDecision) => void;
  onCancel: () => void;
  onOpenChat: () => void;
}) {
  const canCancel = !isTerminal(execution.state);
  const awaitingApproval = execution.state === "WaitingForApproval";
  const waitingStep = execution.steps.find(
    (step) =>
      step.state === "WaitingForApproval" || step.state === "WaitingForInput",
  );

  return (
    <div className="mx-auto max-w-6xl p-5">
      <div className="flex flex-wrap items-start justify-between gap-4">
        <div className="min-w-0">
          <div className="flex items-center gap-2">
            <h2 className="text-lg font-semibold">
              {execution.definition.name}
            </h2>
            <ExecutionStateBadge state={execution.state} />
          </div>
          <p className="mt-1 break-all font-mono text-[10px] text-subtle">
            {execution.id}
          </p>
        </div>
        <div className="flex flex-wrap gap-2">
          {execution.chatId ? (
            <Button disabled={!execution.podId} onClick={onOpenChat}>
              <Bot aria-hidden="true" size={13} /> Open agent
            </Button>
          ) : null}
          {awaitingApproval ? (
            <>
              <Button
                variant="primary"
                disabled={Boolean(pendingAction)}
                onClick={() => onResolve("Approve")}
              >
                {pendingAction === "Approve" ? (
                  <LoaderCircle
                    aria-hidden="true"
                    className="animate-spin"
                    size={13}
                  />
                ) : (
                  <Check aria-hidden="true" size={13} />
                )}
                Approve
              </Button>
              <Button
                variant="danger"
                disabled={Boolean(pendingAction)}
                onClick={() => onResolve("Reject")}
              >
                {pendingAction === "Reject" ? (
                  <LoaderCircle
                    aria-hidden="true"
                    className="animate-spin"
                    size={13}
                  />
                ) : (
                  <X aria-hidden="true" size={13} />
                )}
                Reject
              </Button>
            </>
          ) : null}
          {canCancel ? (
            <Button
              variant="danger"
              disabled={Boolean(pendingAction)}
              onClick={onCancel}
            >
              {pendingAction === "cancel" ? (
                <LoaderCircle
                  aria-hidden="true"
                  className="animate-spin"
                  size={13}
                />
              ) : (
                <CircleStop aria-hidden="true" size={13} />
              )}
              Cancel
            </Button>
          ) : null}
        </div>
      </div>

      {waitingStep ? (
        <section className="mt-4 rounded-lg border border-amber-500/25 bg-amber-500/[0.06] p-3.5">
          <div className="flex items-center gap-2 text-xs font-semibold text-amber-200">
            {waitingStep.state === "WaitingForApproval" ? (
              <ShieldCheck aria-hidden="true" size={14} />
            ) : (
              <Bot aria-hidden="true" size={14} />
            )}
            {waitingStep.definition.name}
          </div>
          <p className="mt-2 text-xs leading-5 text-muted">
            {waitingStep.definition.kind.type === "Approval"
              ? waitingStep.definition.kind.prompt
              : "The Automation agent is waiting for human input in its chat."}
          </p>
        </section>
      ) : null}

      {execution.error ? (
        <p
          className="mt-4 rounded-lg border border-red-500/20 bg-red-500/5 p-3 text-xs text-red-200"
          role="alert"
        >
          {execution.error}
        </p>
      ) : null}

      <section className="mt-4 rounded-lg border border-ui-border bg-surface/50 p-3.5">
        <h3 className="text-xs font-semibold">Run</h3>
        <dl className="mt-3 grid grid-cols-[90px_minmax(0,1fr)] gap-x-4 gap-y-2 text-xs">
          <dt className="text-subtle">Trigger</dt>
          <dd>
            {execution.trigger.type === "Manual"
              ? "Manual"
              : `Schedule · ${execution.trigger.cron}`}
          </dd>
          <dt className="text-subtle">Created</dt>
          <dd>{formatTimestamp(execution.createdAt)}</dd>
          <dt className="text-subtle">Started</dt>
          <dd>
            {execution.startedAt ? formatTimestamp(execution.startedAt) : "—"}
          </dd>
          <dt className="text-subtle">Finished</dt>
          <dd>
            {execution.finishedAt ? formatTimestamp(execution.finishedAt) : "—"}
          </dd>
          <dt className="text-subtle">Pod</dt>
          <dd className="break-all font-mono text-[11px]">
            {execution.podId ?? "—"}
          </dd>
        </dl>
      </section>

      <StepExecutions steps={execution.steps} />
      <ExecutionOutput executionId={execution.id} />
    </div>
  );
}

function DefinitionMetadata({
  definition,
}: {
  definition: automations.AutomationDefinition;
}) {
  const triggerLabels = definition.triggers.map((trigger) =>
    trigger.type === "Manual" ? "manual" : `${trigger.cron} UTC`,
  );
  return (
    <section className="mt-4 rounded-lg border border-ui-border bg-surface/50 p-3.5">
      <dl className="grid grid-cols-[90px_minmax(0,1fr)] gap-x-4 gap-y-2 text-xs">
        <dt className="text-subtle">File ID</dt>
        <dd className="font-mono">{definition.id}</dd>
        <dt className="text-subtle">Triggers</dt>
        <dd>{triggerLabels.join(", ")}</dd>
        <dt className="text-subtle">Concurrency</dt>
        <dd>{definition.maxConcurrent}</dd>
        <dt className="text-subtle">Timeout</dt>
        <dd>
          {definition.timeoutSeconds
            ? formatDuration(Number(definition.timeoutSeconds))
            : "none"}
        </dd>
        {definition.agentDefaults ? (
          <>
            <dt className="text-subtle">Agent</dt>
            <dd>
              {harnessLabel(definition.agentDefaults.harness)}
              {definition.agentDefaults.model
                ? ` · ${definition.agentDefaults.model.model}`
                : " · harness default model"}
            </dd>
          </>
        ) : null}
      </dl>
    </section>
  );
}

function StepDefinitions({
  steps,
}: {
  steps: readonly automations.AutomationStepDefinition[];
}) {
  return (
    <section className="mt-4">
      <h3 className="mb-2 text-xs font-semibold">Steps</h3>
      <div className="space-y-2">
        {steps.map((step, index) => (
          <div
            className="rounded-lg border border-ui-border bg-surface/40 p-3"
            key={step.id}
          >
            <div className="flex items-center gap-2">
              <span className="w-5 shrink-0 font-mono text-[10px] text-subtle">
                {index + 1}.
              </span>
              <span className="text-xs font-medium">{step.name}</span>
              <Badge className="ml-auto" size="xs">
                {stepKindLabel(step.kind)}
              </Badge>
            </div>
            <p className="mt-2 line-clamp-3 whitespace-pre-wrap font-mono text-[11px] leading-5 text-muted">
              {stepDescription(step.kind)}
            </p>
          </div>
        ))}
      </div>
    </section>
  );
}

function StepExecutions({
  steps,
}: {
  steps: readonly automations.AutomationStepExecution[];
}) {
  return (
    <section className="mt-4">
      <h3 className="mb-2 text-xs font-semibold">Steps</h3>
      <div className="space-y-2">
        {steps.map((step, index) => (
          <div
            className="rounded-lg border border-ui-border bg-surface/40 p-3"
            key={step.definition.id}
          >
            <div className="flex flex-wrap items-center gap-2">
              <span className="w-5 shrink-0 font-mono text-[10px] text-subtle">
                {index + 1}.
              </span>
              <span className="text-xs font-medium">
                {step.definition.name}
              </span>
              <Badge size="xs">{stepKindLabel(step.definition.kind)}</Badge>
              <StepStateBadge state={step.state} />
            </div>
            {step.error ? (
              <p className="mt-2 text-xs text-red-200">{step.error}</p>
            ) : null}
            {step.hostOperationId ? (
              <p className="mt-2 break-all font-mono text-[10px] text-subtle">
                Host operation: {step.hostOperationId}
              </p>
            ) : null}
            {step.approvalResolution ? (
              <p className="mt-2 text-[10px] text-subtle">
                {step.approvalResolution.decision === "Approve"
                  ? "Approved"
                  : "Rejected"}{" "}
                by {actorLabel(step.approvalResolution.resolvedBy)} ·{" "}
                {formatTimestamp(step.approvalResolution.resolvedAt)}
              </p>
            ) : null}
          </div>
        ))}
      </div>
    </section>
  );
}

function ExecutionOutput({
  executionId,
}: {
  executionId: automations.AutomationExecutionId;
}) {
  const [lines, setLines] = useState<automations.AutomationOutputLine[]>([]);
  const [caughtUp, setCaughtUp] = useState(false);
  const [error, setError] = useState<string>();

  useEffect(() => {
    setLines([]);
    setCaughtUp(false);
    setError(undefined);
    return hostApi.subscribe(
      "automations_Output",
      { executionId },
      {
        onEvent: (event) => {
          if (event.update.type === "CaughtUp") {
            setCaughtUp(true);
            return;
          }
          const line = event.update;
          setLines((current) => {
            if (
              current.some(
                (currentLine) =>
                  String(currentLine.sequence) === String(line.sequence),
              )
            ) {
              return current;
            }
            return [...current, line]
              .toSorted(compareOutputSequence)
              .slice(-MAX_VISIBLE_OUTPUT_LINES);
          });
        },
        onError: (cause) => setError(cause.message),
      },
    );
  }, [executionId]);

  return (
    <section className="mt-4 overflow-hidden rounded-lg border border-ui-border bg-[#0b0d10]">
      <div className="flex items-center justify-between border-b border-ui-border px-3.5 py-2.5">
        <h3 className="text-xs font-semibold">Output</h3>
        <span
          className="text-[10px] text-subtle"
          role={error ? "alert" : "status"}
        >
          {error ??
            (caughtUp
              ? `${lines[0] && String(lines[0].sequence) !== "1" ? "latest " : ""}${lines.length} retained lines`
              : "Loading…")}
        </span>
      </div>
      <div className="max-h-[420px] overflow-auto">
        {lines.length ? (
          <pre className="min-w-max p-3.5 font-mono text-[11px] leading-5 text-muted">
            {lines.map((line) => (
              <span className="block" key={String(line.sequence)}>
                <span className="mr-3 select-none text-subtle/60">
                  {String(line.sequence).padStart(5, " ")}
                </span>
                <span className={outputTone(line.source)}>{line.content}</span>
              </span>
            ))}
          </pre>
        ) : (
          <p className="p-4 text-xs text-subtle">
            {caughtUp ? "No output retained." : "Loading output…"}
          </p>
        )}
      </div>
    </section>
  );
}

function ExecutionStateBadge({
  state,
}: {
  state: automations.AutomationExecutionState;
}) {
  const presentation = executionStatePresentation(state);
  const Icon = presentation.icon;
  return (
    <Badge className="shrink-0 gap-1" size="xs" tone={presentation.tone}>
      <Icon
        aria-hidden="true"
        className={state === "Running" ? "animate-spin" : undefined}
        size={10}
      />
      {presentation.label}
    </Badge>
  );
}

function StepStateBadge({ state }: { state: automations.AutomationStepState }) {
  const tone: BadgeTone =
    state === "Succeeded"
      ? "success"
      : state === "Failed"
        ? "danger"
        : state === "WaitingForApproval" || state === "WaitingForInput"
          ? "warning"
          : state === "Running"
            ? "primary"
            : "muted";
  return (
    <Badge className="ml-auto" size="xs" tone={tone}>
      {splitWords(state)}
    </Badge>
  );
}

function executionStatePresentation(
  state: automations.AutomationExecutionState,
): {
  label: string;
  tone: BadgeTone;
  icon: typeof Clock3;
} {
  switch (state) {
    case "Queued":
      return { label: "Queued", tone: "muted", icon: Clock3 };
    case "Running":
      return { label: "Running", tone: "primary", icon: LoaderCircle };
    case "WaitingForApproval":
      return { label: "Approval", tone: "warning", icon: ShieldCheck };
    case "WaitingForInput":
      return { label: "Input", tone: "warning", icon: Bot };
    case "Succeeded":
      return { label: "Succeeded", tone: "success", icon: Check };
    case "Failed":
      return { label: "Failed", tone: "danger", icon: X };
    case "Canceled":
      return { label: "Canceled", tone: "muted", icon: CircleStop };
    case "Interrupted":
      return { label: "Interrupted", tone: "warning", icon: CircleStop };
  }
}

function hasManualTrigger(
  definition: automations.AutomationDefinition,
): boolean {
  return definition.triggers.some((trigger) => trigger.type === "Manual");
}

function isTerminal(state: automations.AutomationExecutionState): boolean {
  return (
    state === "Succeeded" ||
    state === "Failed" ||
    state === "Canceled" ||
    state === "Interrupted"
  );
}

function stepKindLabel(kind: automations.AutomationStepKind): string {
  return kind.type === "HostCommand" ? "host command" : kind.type.toLowerCase();
}

function stepDescription(kind: automations.AutomationStepKind): string {
  switch (kind.type) {
    case "Command":
      return kind.run;
    case "Agent":
      return kind.prompt;
    case "HostCommand":
      return kind.command;
    case "Approval":
      return kind.prompt;
  }
}

function harnessLabel(
  harness: automations.AutomationAgentSelection["harness"],
): string {
  return harness === "ClaudeCode" ? "Claude Code" : harness;
}

function actorLabel(actor: protocol.Actor): string {
  switch (actor.type) {
    case "Client":
      return `client ${actor.clientId}`;
    case "Host":
      return "host";
    case "Workspace":
      return `workspace ${actor.workspace}`;
    case "Pod":
      return `pod ${actor.podId}`;
  }
}

function formatTimestamp(timestamp: string): string {
  return new Date(timestamp).toLocaleString();
}

function formatDuration(seconds: number): string {
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes} min`;
  const hours = minutes / 60;
  return Number.isInteger(hours) ? `${hours} h` : `${hours.toFixed(1)} h`;
}

function splitWords(value: string): string {
  return value.replaceAll(/([a-z])([A-Z])/g, "$1 $2");
}

function compareOutputSequence(
  left: automations.AutomationOutputLine,
  right: automations.AutomationOutputLine,
): number {
  const leftSequence = BigInt(String(left.sequence));
  const rightSequence = BigInt(String(right.sequence));
  return leftSequence < rightSequence
    ? -1
    : leftSequence > rightSequence
      ? 1
      : 0;
}

function outputTone(source: automations.AutomationOutputSource): string {
  if (source.type === "Automation") return "text-accent-text";
  return source.content.type === "Stderr" ? "text-amber-200" : "text-muted";
}

function errorMessage(cause: unknown): string {
  return cause instanceof Error ? cause.message : String(cause);
}
