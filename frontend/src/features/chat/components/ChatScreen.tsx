import {
  ChevronDown,
  ArrowDown,
  LoaderCircle,
  Trash2,
} from "lucide-react";
import { useCallback, useMemo, useRef, useState } from "react";

import type { chats, config } from "../../../api/generated/index.ts";
import { Button } from "../../../components/ui/Button.tsx";
import {
  SelectControl,
  type SelectControlOption,
} from "../../../components/ui/SelectControl.tsx";
import type { ChatScreenProps } from "../types.ts";
import { chatTimeline, chatTurns } from "../model/replicas.ts";
import { ChatComposer } from "./ChatComposer.tsx";
import { ChatStatusBar } from "./ChatStatusBar.tsx";
import { ChatTimeline } from "./ChatTimeline.tsx";

export function ChatScreen({
  summary,
  replica,
  harness,
  modelPreferences,
  usageSettings,
  slashCommands,
  actions,
  attachmentUploader,
  attachmentUrl,
  onError,
  showUnknownItems = false,
}: ChatScreenProps) {
  const [busy, setBusy] = useState<string>();
  const [resolvingRequestId, setResolvingRequestId] = useState<chats.ChatRequestId>();
  const timelineScroll = useRef<HTMLElement>(null);
  const [followingTimeline, setFollowingTimeline] = useState(
    () => !hasStoredNonFollowState(summary.chatId),
  );
  const turns = useMemo(
    () => replica ? chatTurns(replica) : [],
    [replica?.turnOrder, replica?.turnsById],
  );
  const timeline = useMemo(
    () => replica ? chatTimeline(replica) : [],
    [replica?.timelineOrder, replica?.timelineById],
  );
  const runningTurn = turns.findLast(
    (turn) => turn.state === "Running" && turn.bindingId === summary.binding?.bindingId,
  );
  const attached = summary.binding?.status === "Attached";
  const detaching = summary.binding?.status === "Detaching";

  const run = async (name: string, action: () => Promise<void>) => {
    setBusy(name);
    try {
      await action();
    } catch (cause) {
      onError(cause);
    } finally {
      setBusy(undefined);
    }
  };

  const onFollowingChange = useCallback((following: boolean) => {
    setFollowingTimeline(following);
    storeNonFollowState(summary.chatId, !following);
  }, [summary.chatId]);

  const queuedPrompts = replica?.queuedPrompts.length ? (
    <QueuedPrompts
      prompts={replica.queuedPrompts}
      flushing={busy === "flush"}
      removingPromptId={busy?.startsWith("remove:") ? busy.slice("remove:".length) : undefined}
      onFlush={() => void run("flush", actions.flushPromptQueue)}
      onRemove={(queuedPromptId) =>
        void run(`remove:${queuedPromptId}`, () => actions.removeQueuedPrompt(queuedPromptId))
      }
    />
  ) : null;

  const composer = (
    <ChatComposer
      draftId={`chat:${summary.chatId}`}
      harness={harness}
      modelPreferences={modelPreferences}
      slashCommands={slashCommands}
      attachmentUploader={attachmentUploader}
      attachmentUrl={attachmentUrl}
      initialModel={summary.model}
      disabledReason={detaching ? "The agent is finishing its current session. You can send when it is ready." : undefined}
      modelLocked={attached && harness?.capabilities.modelSwitching === "Unsupported"}
      interrupting={Boolean(runningTurn) && attached}
      onInterrupt={actions.interrupt}
      onError={onError}
      onSubmit={async (submission) => {
        await actions.sendPrompt(submission);
      }}
    />
  );

  return (
    <div className="flex h-full min-h-0 w-full flex-1 flex-col overflow-hidden bg-canvas">
      <section
        className="relative min-h-0 flex-1 overflow-x-hidden overflow-y-auto overscroll-contain"
        data-chat-scroll
        ref={timelineScroll}
      >
        <div className="flex min-h-full flex-col">
          {!replica ? (
            <div className="grid flex-1 place-items-center p-8">
              <div className="flex items-center gap-3 text-sm text-muted">
                <LoaderCircle className="size-4 animate-spin text-accent" /> Loading chat…
              </div>
            </div>
          ) : (
            <ChatTimeline
              entries={timeline}
              replica={replica}
              summary={summary}
              runningTurn={runningTurn}
              resolvingRequestId={resolvingRequestId}
              showStatus={false}
              showUnknownItems={showUnknownItems}
              attachmentUrl={attachmentUrl}
              initialFollowing={followingTimeline}
              onFollowingChange={onFollowingChange}
              onResolveRequest={async (requestId, answers) => {
                setResolvingRequestId(requestId);
                try {
                  await actions.resolveRequest(requestId, answers);
                } catch (cause) {
                  onError(cause);
                } finally {
                  setResolvingRequestId(undefined);
                }
              }}
            />
          )}
          <ComposerFooter
            onJumpToLatest={!followingTimeline
              ? () => timelineScroll.current?.scrollTo({
                  top: timelineScroll.current.scrollHeight,
                  behavior: "smooth",
                })
              : undefined}
          >
            {queuedPrompts}
            <ChatCostCenterControl
              summary={summary}
              usageSettings={usageSettings}
              disabled={Boolean(busy)}
              onChange={(costCenterId) =>
                void run("cost-center", () => actions.setCostCenter(costCenterId))
              }
            />
            <ChatStatusBar summary={summary} replica={replica} runningTurn={runningTurn} />
            {composer}
          </ComposerFooter>
        </div>
      </section>
    </div>
  );
}

const UNASSIGNED_COST_CENTER = ":unassigned";

function ChatCostCenterControl({
  summary,
  usageSettings,
  disabled,
  onChange,
}: {
  summary: chats.ChatSummary;
  usageSettings?: config.WorkspaceUsageSettings;
  disabled: boolean;
  onChange: (costCenterId?: chats.ChatCostCenterId) => void;
}) {
  const options: SelectControlOption[] = Object.entries(usageSettings?.costCenters ?? {})
    .filter((entry): entry is [string, config.WorkspaceCostCenter] =>
      entry[1] !== undefined && entry[1].archived !== true
    )
    .map(([id, costCenter]) => ({ value: id, label: costCenter.name }))
    .sort((left, right) => left.label.localeCompare(right.label));
  if (
    summary.costCenterId
    && !options.some((option) => option.value === summary.costCenterId)
  ) {
    const configured = usageSettings?.costCenters?.[summary.costCenterId];
    options.push({
      value: summary.costCenterId,
      label: configured?.name ?? summary.costCenterId,
      badge: { label: configured?.archived === true ? "Archived" : "Unconfigured" },
    });
  }
  if (!options.length && !summary.costCenterId) return null;

  return (
    <div className="flex justify-end px-3.5 pt-2">
      <SelectControl
        className="w-48"
        hideLabel
        label="Chat cost center"
        value={summary.costCenterId ?? UNASSIGNED_COST_CENTER}
        options={[
          { value: UNASSIGNED_COST_CENTER, label: "Unassigned" },
          ...options,
        ]}
        disabled={disabled}
        onChange={(value) => onChange(
          value === UNASSIGNED_COST_CENTER
            ? undefined
            : value as chats.ChatCostCenterId,
        )}
      />
    </div>
  );
}

function hasStoredNonFollowState(chatId: chats.ChatId): boolean {
  try {
    return window.localStorage.getItem(nonFollowStorageKey(chatId)) === "1";
  } catch {
    return false;
  }
}

function storeNonFollowState(chatId: chats.ChatId, nonFollowing: boolean): void {
  try {
    if (nonFollowing) {
      window.localStorage.setItem(nonFollowStorageKey(chatId), "1");
    } else {
      window.localStorage.removeItem(nonFollowStorageKey(chatId));
    }
  } catch {
    // Timeline preference persistence is best-effort when browser storage is unavailable.
  }
}

function nonFollowStorageKey(chatId: chats.ChatId): string {
  return `tascarrel.chat.timeline.non-follow.v1.${chatId}`;
}

function ComposerFooter({
  children,
  onJumpToLatest,
}: {
  children: React.ReactNode;
  onJumpToLatest?: () => void;
}) {
  return (
    <footer className="chat-composer-footer sticky bottom-0 z-20 max-h-[45dvh] flex-none overflow-visible bg-canvas px-3 pb-4 pt-0 sm:px-6 sm:pb-6 sm:pt-0">
      <div
        aria-hidden="true"
        className="pointer-events-none absolute bottom-full left-1/2 h-16 w-[calc(100%-1.5rem)] max-w-4xl -translate-x-1/2 bg-gradient-to-t from-canvas to-transparent sm:w-[calc(100%-3rem)]"
      />
      {onJumpToLatest ? (
        <Button
          aria-label="Jump to latest message"
          className="absolute bottom-[calc(100%+1rem)] left-1/2 z-30 size-9 -translate-x-1/2 rounded-full p-0 shadow-lg shadow-black/20 hover:border-accent/50 hover:text-foreground"
          size="icon"
          title="Jump to latest message"
          onClick={onJumpToLatest}
        >
          <ArrowDown aria-hidden="true" className="size-4" />
        </Button>
      ) : null}
      <div className="chat-composer-content mx-auto max-h-[calc(45dvh-3.25rem)] max-w-4xl overflow-y-auto">{children}</div>
    </footer>
  );
}

function QueuedPrompts({
  prompts,
  flushing,
  removingPromptId,
  onFlush,
  onRemove,
}: {
  prompts: chats.ChatQueuedPrompt[];
  flushing: boolean;
  removingPromptId?: string;
  onFlush: () => void;
  onRemove: (queuedPromptId: chats.ChatQueuedPromptId) => void;
}) {
  return (
    <details className="group mb-2 w-full overflow-hidden rounded-2xl border border-ui-border-strong bg-surface">
      <summary className="flex cursor-pointer list-none items-center gap-2 px-3.5 py-2.5 text-xs text-muted transition hover:bg-surface-raised [&::-webkit-details-marker]:hidden">
        <ChevronDown className="size-3.5 -rotate-90 transition group-open:rotate-0" />
        <span className="grid size-5 place-items-center rounded-md bg-accent/10 text-[10px] font-semibold text-accent-text">
          {prompts.length}
        </span>
        Queued {prompts.length === 1 ? "prompt" : "prompts"}
      </summary>
      <div className="border-t border-ui-border px-3.5 py-2.5">
        <ol className="space-y-1.5 text-xs text-subtle">
          {prompts.map((queued) => (
            <li className="flex min-w-0 items-center gap-2" key={queued.queuedPromptId}>
              <span className="min-w-0 flex-1 truncate">
                {queued.prompt.text || `${queued.prompt.attachments.length} attachment(s)`}
              </span>
              <Button
                aria-label="Remove queued prompt"
                className="size-5 shrink-0 rounded-md border-0 bg-transparent p-1 text-subtle hover:bg-red-500/10 hover:text-red-300"
                size="icon"
                title="Remove queued prompt"
                disabled={flushing || Boolean(removingPromptId)}
                onClick={() => onRemove(queued.queuedPromptId)}
              >
                {removingPromptId === queued.queuedPromptId ? (
                  <LoaderCircle className="size-3 animate-spin" />
                ) : (
                  <Trash2 className="size-3" />
                )}
              </Button>
            </li>
          ))}
        </ol>
        <Button
          className="mt-2"
          size="small"
          variant="primary"
          disabled={flushing || Boolean(removingPromptId)}
          onClick={onFlush}
        >
          {flushing ? "Sending…" : "Send Now"}
        </Button>
      </div>
    </details>
  );
}
