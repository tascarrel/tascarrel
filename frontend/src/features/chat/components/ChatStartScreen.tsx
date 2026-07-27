import { ArrowRight } from "lucide-react";
import { useEffect, useMemo, useState } from "react";

import type { chats, config } from "../../../api/generated/index.ts";
import { Button } from "../../../components/ui/Button.tsx";
import {
  loadChatComposerDraft,
  loadChatCreatorDraft,
  removeChatComposerDraft,
  removeChatCreatorDraft,
  storeChatCreatorDraft,
} from "../model/drafts.ts";
import { harnessKindKey } from "../model/format.ts";
import {
  chatModelPreferences,
  configuredChatHarness,
} from "../model/modelPreferences.ts";
import type {
  AttachmentUploader,
  AttachmentUrlResolver,
  StartChatSubmission,
} from "../types.ts";
import { ChatComposer } from "./ChatComposer.tsx";

export function ChatStartScreen({
  draftId,
  harnesses,
  settings,
  slashCommands,
  creationTarget = "chat",
  loading = false,
  attachmentUploader,
  attachmentUrl,
  onStart,
  onCreateWithoutChat,
  onError,
}: {
  draftId: string;
  harnesses: readonly chats.ChatHarness[];
  settings?: config.WorkspaceSettings;
  slashCommands?: config.WorkspaceChatConfig["commands"];
  creationTarget?: "chat" | "pod";
  loading?: boolean;
  attachmentUploader?: AttachmentUploader;
  attachmentUrl?: AttachmentUrlResolver;
  onStart: (submission: StartChatSubmission) => Promise<void>;
  onCreateWithoutChat?: (title: string) => Promise<void>;
  onError: (cause: unknown) => void;
}) {
  const creatingPod = creationTarget === "pod";
  const composerDraftId = `creator:${draftId}`;
  const [restoredDraft] = useState(() => loadChatCreatorDraft(draftId));
  const [harnessKey, setHarnessKey] = useState<string | undefined>(restoredDraft?.harnessKey);
  const [title, setTitle] = useState(restoredDraft?.title ?? "");
  const [promptEmpty, setPromptEmpty] = useState(() => {
    const composerDraft = loadChatComposerDraft(composerDraftId);
    return !composerDraft?.text.trim() && !composerDraft?.attachments.length;
  });
  const [startingTarget, setStartingTarget] = useState<"chat" | "pod">();
  const configuredHarnesses = useMemo(
    () => harnesses.map((harness) => configuredChatHarness(harness, settings)),
    [harnesses, settings],
  );
  const authenticatedHarnesses = useMemo(
    () => configuredHarnesses.filter((candidate) => candidate.credentials.state === "Valid"),
    [configuredHarnesses],
  );
  const harness = useMemo(
    () => authenticatedHarnesses.find(
      (candidate) => harnessKindKey(candidate.kind) === harnessKey,
    ) ?? authenticatedHarnesses.find(
      (candidate) => candidate.kind === settings?.chat?.defaultHarness,
    ) ?? authenticatedHarnesses[0],
    [authenticatedHarnesses, harnessKey, settings?.chat?.defaultHarness],
  );

  useEffect(() => {
    if (harness && harnessKindKey(harness.kind) !== harnessKey) {
      setHarnessKey(harnessKindKey(harness.kind));
    }
  }, [harness, harnessKey]);

  useEffect(() => {
    storeChatCreatorDraft(draftId, { title, ...(harnessKey ? { harnessKey } : {}) });
  }, [draftId, harnessKey, title]);

  const createWithoutChat = async () => {
    const explicitTitle = title.trim();
    if (
      !creatingPod
      || !onCreateWithoutChat
      || !explicitTitle
      || !promptEmpty
      || startingTarget
    ) return;
    setStartingTarget("pod");
    try {
      await onCreateWithoutChat(explicitTitle);
      removeChatCreatorDraft(draftId);
      removeChatComposerDraft(composerDraftId);
    } catch (cause) {
      onError(cause);
    } finally {
      setStartingTarget(undefined);
    }
  };

  return (
    <div className="relative flex min-h-0 w-full flex-1 flex-col overflow-y-auto bg-canvas px-4 py-12 sm:px-8">
        <div className="relative z-10 flex min-h-0 w-full max-w-3xl flex-1 flex-col justify-end self-center">
          <div className="mb-7 text-center">
            <h1 className="text-3xl font-semibold tracking-[-0.035em] text-foreground sm:text-4xl">
              What Should We Build?
            </h1>
            <p className="mx-auto mt-3 max-w-xl text-sm leading-6 text-muted">
              {creatingPod
                ? "Start with a task, question, or idea. Tascarrel creates the pod and its first chat, then sends your message when the environment is ready."
                : "Start with a task, question, or idea. Tascarrel creates the chat, attaches the selected harness, and sends your message in one step."}
            </p>
          </div>

          <div className="mb-3 flex flex-wrap items-center justify-between gap-3 px-1">
            <div className="flex flex-wrap gap-2" role="group" aria-label="Harness">
              {authenticatedHarnesses.map((candidate) => {
                const key = harnessKindKey(candidate.kind);
                const selected = key === harnessKindKey(harness?.kind ?? candidate.kind);
                return (
                  <Button
                    className={`h-auto rounded-xl px-3 py-2 ${
                      selected
                        ? "border-accent/40 bg-accent/10 text-accent-text"
                        : "border-ui-border bg-surface text-muted hover:border-ui-border-strong"
                    }`}
                    key={key}
                    size="small"
                    aria-pressed={selected}
                    onClick={() => setHarnessKey(key)}
                  >
                    {candidate.displayName}
                  </Button>
                );
              })}
              {!authenticatedHarnesses.length ? (
                <span className="rounded-xl border border-ui-border bg-surface px-3 py-2 text-xs text-subtle">
                  {loading ? "Discovering harnesses…" : "No authenticated harnesses"}
                </span>
              ) : null}
            </div>
          </div>
        </div>

        <div className="relative z-10 w-full max-w-3xl shrink-0 self-center">
          <ChatComposer
            key={harness ? harnessKindKey(harness.kind) : "no-harness"}
            draftId={composerDraftId}
            harness={harness}
            modelPreferences={harness ? chatModelPreferences(settings, harness.kind) : undefined}
            slashCommands={slashCommands}
            attachmentUploader={attachmentUploader}
            attachmentUrl={attachmentUrl}
            autoFocus
            showDeliveryMode={false}
            submitLabel={creatingPod ? "Create pod and start chat" : "Start chat"}
            disabledReason={startingTarget
              ? startingTarget === "pod"
                ? "Creating your pod…"
                : creatingPod
                  ? "Creating your pod and chat…"
                  : "Creating your chat…"
              : !harness
                ? "Authenticate a harness in Settings before starting a chat."
                : undefined}
            onError={onError}
            onPromptEmptyChange={setPromptEmpty}
            onSubmit={async ({ prompt }) => {
              if (!harness) return;
              const explicitTitle = title.trim();
              setStartingTarget("chat");
              try {
                await onStart({
                  harness: harness.kind,
                  ...(explicitTitle ? { title: explicitTitle } : {}),
                  ...(prompt.model ? { model: prompt.model } : {}),
                  prompt,
                });
                removeChatCreatorDraft(draftId);
              } finally {
                setStartingTarget(undefined);
              }
            }}
          />
        </div>

        <div className="relative z-10 min-h-0 w-full max-w-3xl flex-1 self-center">
          <details className="group mx-auto mt-4 max-w-xl text-center">
            <summary className="inline-flex cursor-pointer list-none items-center gap-1.5 text-xs text-subtle hover:text-muted [&::-webkit-details-marker]:hidden">
              {creatingPod ? "Name this pod" : "Name this chat"}
              <ArrowRight aria-hidden="true" className="size-3 transition group-open:rotate-90" />
            </summary>
            <div className="mt-3 flex flex-col gap-2 sm:flex-row">
              <input
                aria-label={creatingPod ? "Pod title" : "Chat title"}
                className="min-w-0 flex-1 rounded-xl border border-ui-border bg-surface px-3 py-2 text-center text-sm text-foreground outline-none placeholder:text-subtle focus:border-accent/50"
                placeholder="A title will be generated from your message"
                value={title}
                disabled={Boolean(startingTarget)}
                onChange={(event) => setTitle(event.target.value)}
              />
              {creatingPod && onCreateWithoutChat ? (
                <Button
                  className="h-auto shrink-0 rounded-xl px-4 py-2 text-sm"
                  disabled={Boolean(startingTarget) || !title.trim() || !promptEmpty}
                  onClick={() => void createWithoutChat()}
                >
                  Create without chat
                </Button>
              ) : null}
            </div>
          </details>
        </div>
    </div>
  );
}
