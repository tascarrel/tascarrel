import { ArrowUp, CircleStop, Paperclip } from "lucide-react";
import {
  type ChangeEvent,
  type ClipboardEvent,
  type DragEvent,
  type KeyboardEvent,
  useEffect,
  useId,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
} from "react";

import type { chats, config } from "../../../api/generated/index.ts";
import { Button } from "../../../components/ui/Button.tsx";
import {
  fuzzySearch,
  type FuzzySearchItem,
} from "../../../components/ui/FuzzySearch.tsx";
import { KeyboardShortcut } from "../../../components/ui/KeyboardShortcut.tsx";
import { SelectControl } from "../../../components/ui/SelectControl.tsx";
import {
  loadChatComposerDraft,
  removeChatComposerDraft,
  storeChatComposerDraft,
} from "../model/drafts.ts";
import { preferredDefaultModelSelection } from "../model/modelPreferences.ts";
import { reconcileModelSelection } from "../model/modelSelection.ts";
import {
  slashCommandQuery,
  type WorkspaceSlashCommand,
  workspaceSlashCommands,
} from "../model/slashCommands.ts";
import type { AttachmentUrlResolver, PromptSubmission } from "../types.ts";
import { AttachmentPreview } from "./AttachmentPreview.tsx";
import { HarnessIcon } from "./HarnessIcon.tsx";
import { ModelControls } from "./ModelControls.tsx";
import {
  SlashCommandMenu,
  slashCommandOptionId,
} from "./SlashCommandMenu.tsx";

export type ChatComposerLayout = "classic" | "compact" | "structured" | "adaptive";

const DELIVERY_OPTIONS = [
  { label: "Queue when busy", value: "WhenIdle" },
  { label: "Steer active turn", value: "Immediate" },
  { label: "Send Now", value: "InterruptAndSend" },
];

const HARNESS_PROVIDERS: Partial<
  Record<chats.ChatHarnessKind, { label: string; href: string }>
> = {
  Codex: {
    label: "Codex App Server",
    href: "https://developers.openai.com/codex/app-server",
  },
  ClaudeCode: {
    label: "Claude Agent SDK",
    href: "https://code.claude.com/docs/en/agent-sdk/overview",
  },
} as const;

export function ChatComposer({
  draftId,
  harness,
  modelPreferences,
  slashCommands,
  initialModel,
  initialText = "",
  disabledReason,
  modelLocked = false,
  showDeliveryMode = true,
  autoFocus = false,
  submitLabel = "Send",
  layout = "classic",
  attachmentUploader,
  attachmentUrl,
  interrupting = false,
  onInterrupt,
  onSubmit,
  onPromptEmptyChange,
  onError,
}: {
  draftId: string;
  harness?: chats.ChatHarness;
  modelPreferences?: config.WorkspaceChatModelPreferences;
  slashCommands?: config.WorkspaceChatConfig["commands"];
  initialModel?: chats.ChatModelSelection;
  initialText?: string;
  disabledReason?: string;
  modelLocked?: boolean;
  showDeliveryMode?: boolean;
  autoFocus?: boolean;
  submitLabel?: string;
  layout?: ChatComposerLayout;
  attachmentUploader?: (file: File) => Promise<chats.ChatPromptAttachment>;
  attachmentUrl?: AttachmentUrlResolver;
  interrupting?: boolean;
  onInterrupt?: () => Promise<void>;
  onSubmit: (submission: PromptSubmission) => Promise<void>;
  onPromptEmptyChange?: (empty: boolean) => void;
  onError: (cause: unknown) => void;
}) {
  const [restoredDraft] = useState(() => loadChatComposerDraft(draftId));
  const [text, setText] = useState(restoredDraft?.text ?? initialText);
  const [attachments, setAttachments] = useState<chats.ChatPromptAttachment[]>(
    restoredDraft?.attachments ?? [],
  );
  const [mode, setMode] = useState<chats.ChatPromptMode>(restoredDraft?.mode ?? "WhenIdle");
  const [model, setModel] = useState<chats.ChatModelSelection | undefined>(() => {
    const preferredDefault = preferredDefaultModelSelection(harness, modelPreferences);
    return reconcileModelSelection(
      harness,
      restoredDraft?.model ?? initialModel ?? preferredDefault,
      preferredDefault,
    );
  });
  const [sending, setSending] = useState(false);
  const [uploadingCount, setUploadingCount] = useState(0);
  const [messageInputFocused, setMessageInputFocused] = useState(false);
  const [commandMenuDismissed, setCommandMenuDismissed] = useState(false);
  const [activeCommandIndex, setActiveCommandIndex] = useState(0);
  const textArea = useRef<HTMLTextAreaElement>(null);
  const fileInput = useRef<HTMLInputElement>(null);
  const restoreFocusAfterSend = useRef(false);
  const commandMenuId = useId();
  const commandItems = useMemo(
    () => workspaceSlashCommands(slashCommands).map((command) => ({
      id: command.name,
      value: command,
      label: `/${command.name}`,
      keywords: [command.name],
    } satisfies FuzzySearchItem<WorkspaceSlashCommand>)),
    [slashCommands],
  );
  const commandQuery = slashCommandQuery(text);
  const matchingCommandItems = useMemo(
    () => commandQuery === undefined ? [] : fuzzySearch(commandItems, commandQuery),
    [commandItems, commandQuery],
  );
  const commandMenuOpen =
    messageInputFocused
    && !commandMenuDismissed
    && commandQuery !== undefined
    && commandItems.length > 0;
  const activeCommandItem = commandMenuOpen
    ? matchingCommandItems[Math.min(activeCommandIndex, matchingCommandItems.length - 1)]
    : undefined;
  const harnessProvider = harness && typeof harness.kind === "string"
    ? HARNESS_PROVIDERS[harness.kind]
    : undefined;
  const harnessVersion = harness?.pinnedVersion
    ? /^[vV]/.test(harness.pinnedVersion) ? harness.pinnedVersion : `v${harness.pinnedVersion}`
    : undefined;

  useEffect(() => {
    setModel((current) => {
      const preferredDefault = preferredDefaultModelSelection(harness, modelPreferences);
      return reconcileModelSelection(
        harness,
        current ?? initialModel ?? preferredDefault,
        preferredDefault,
      );
    });
  }, [harness, initialModel, modelPreferences]);

  useEffect(() => {
    storeChatComposerDraft(draftId, { text, attachments, mode, ...(model ? { model } : {}) });
  }, [attachments, draftId, mode, model, text]);

  useEffect(() => {
    onPromptEmptyChange?.(
      !text.trim() && attachments.length === 0 && uploadingCount === 0,
    );
  }, [attachments.length, onPromptEmptyChange, text, uploadingCount]);

  useEffect(() => {
    if (sending || !restoreFocusAfterSend.current) return;
    restoreFocusAfterSend.current = false;
    textArea.current?.focus();
  }, [sending]);

  useEffect(() => setActiveCommandIndex(0), [commandItems, commandQuery]);

  useLayoutEffect(() => {
    const input = textArea.current;
    if (!input) return;

    const previousScrollTop = input.scrollTop;
    const cursorAtEnd = input.selectionEnd === input.value.length;
    const maxHeight = Math.min(TEXTAREA_MAX_HEIGHT, window.innerHeight * 0.25);

    input.style.height = "0px";
    input.style.height = `${Math.min(input.scrollHeight, maxHeight)}px`;

    if (input.scrollHeight > maxHeight) {
      input.scrollTop = cursorAtEnd ? input.scrollHeight : previousScrollTop;
    }
  }, [text]);

  const submit = async (deliveryMode = mode, submittedText = text) => {
    if (
      (!submittedText.trim() && attachments.length === 0)
      || disabledReason
      || sending
      || uploadingCount > 0
    ) return;
    setSending(true);
    try {
      await onSubmit({
        prompt: {
          ...(submittedText.trim() ? { text: submittedText.trim() } : {}),
          attachments,
          ...(model ? { model } : {}),
        },
        mode: deliveryMode,
      });
      removeChatComposerDraft(draftId);
      setText("");
      setAttachments([]);
      restoreFocusAfterSend.current = true;
    } catch (cause) {
      onError(cause);
    } finally {
      setSending(false);
    }
  };

  const expandSlashCommand = (command: WorkspaceSlashCommand) => {
    setText(command.text);
    setCommandMenuDismissed(true);
    requestAnimationFrame(() => {
      textArea.current?.focus();
      textArea.current?.setSelectionRange(command.text.length, command.text.length);
    });
  };

  const sendSlashCommand = (command: WorkspaceSlashCommand, deliveryMode: chats.ChatPromptMode) => {
    setText(command.text);
    setCommandMenuDismissed(true);
    void submit(deliveryMode, command.text);
  };

  const onKeyDown = (event: KeyboardEvent<HTMLTextAreaElement>) => {
    if (commandMenuOpen) {
      if (event.key === "ArrowDown" && matchingCommandItems.length > 0) {
        event.preventDefault();
        setActiveCommandIndex((current) => (current + 1) % matchingCommandItems.length);
        return;
      }
      if (event.key === "ArrowUp" && matchingCommandItems.length > 0) {
        event.preventDefault();
        setActiveCommandIndex(
          (current) => (current - 1 + matchingCommandItems.length) % matchingCommandItems.length,
        );
        return;
      }
      if (event.key === "Tab" && activeCommandItem) {
        event.preventDefault();
        expandSlashCommand(activeCommandItem.value);
        return;
      }
      if (event.key === "Enter" && activeCommandItem) {
        event.preventDefault();
        if (event.shiftKey && !event.metaKey && !event.ctrlKey && !event.altKey) {
          expandSlashCommand(activeCommandItem.value);
        } else {
          const modified = event.metaKey || event.ctrlKey;
          sendSlashCommand(
            activeCommandItem.value,
            modified && event.altKey
              ? "InterruptAndSend"
              : modified && event.shiftKey
                ? "Immediate"
                : "WhenIdle",
          );
        }
        return;
      }
      if (event.key === "Escape" && !event.repeat) {
        event.preventDefault();
        setCommandMenuDismissed(true);
        return;
      }
    }
    if (event.key === "Enter" && (event.metaKey || event.ctrlKey)) {
      event.preventDefault();
      void submit(
        event.altKey
          ? "InterruptAndSend"
          : event.shiftKey
            ? "Immediate"
            : "WhenIdle",
      );
      return;
    }
    if (event.key === "Enter" && !event.shiftKey) {
      event.preventDefault();
      void submit("WhenIdle");
      return;
    }
    if (event.key === "Escape" && !event.repeat && interrupting && onInterrupt) {
      event.preventDefault();
      void onInterrupt().catch(onError);
    }
  };

  const uploadFiles = async (files: File[]) => {
    if (!attachmentUploader || files.length === 0) return;
    setUploadingCount((current) => current + files.length);
    try {
      const results = await Promise.allSettled(files.map(attachmentUploader));
      const uploaded = results.flatMap((result) =>
        result.status === "fulfilled" ? [result.value] : []
      );
      if (uploaded.length > 0) {
        setAttachments((current) => {
          const known = new Set(current.map((attachment) => attachment.attachmentId));
          return [...current, ...uploaded.filter((attachment) => !known.has(attachment.attachmentId))];
        });
      }
      for (const result of results) {
        if (result.status === "rejected") onError(result.reason);
      }
    } finally {
      setUploadingCount((current) => Math.max(0, current - files.length));
    }
  };

  const onFilesSelected = (event: ChangeEvent<HTMLInputElement>) => {
    const files = Array.from(event.target.files ?? []);
    event.target.value = "";
    void uploadFiles(files);
  };

  const onPaste = (event: ClipboardEvent<HTMLTextAreaElement>) => {
    if (!attachmentUploader) return;
    const files = Array.from(event.clipboardData.files);
    if (files.length > 0) {
      event.preventDefault();
      void uploadFiles(files);
      return;
    }
    const pasted = event.clipboardData.getData("text/plain");
    if (pasted.length < LARGE_PASTE_CHARACTERS) return;
    event.preventDefault();
    const markdown = looksLikeMarkdown(pasted);
    void uploadFiles([
      new File(
        [pasted],
        `pasted-text-${timestampForName()}.${markdown ? "md" : "txt"}`,
        { type: markdown ? "text/markdown" : "text/plain" },
      ),
    ]);
  };

  const onDrop = (event: DragEvent<HTMLDivElement>) => {
    if (!attachmentUploader || event.dataTransfer.files.length === 0) return;
    event.preventDefault();
    void uploadFiles(Array.from(event.dataTransfer.files));
  };

  const sendDisabled =
    Boolean(disabledReason)
    || sending
    || uploadingCount > 0
    || (!text.trim() && attachments.length === 0);
  const frameEvents = {
    onDragOver: (event: DragEvent<HTMLDivElement>) => {
      if (attachmentUploader && event.dataTransfer.types.includes("Files")) event.preventDefault();
    },
    onDrop,
  };
  const filePicker = (
    <input
      ref={fileInput}
      className="hidden"
      type="file"
      multiple
      accept="image/*,application/pdf,text/*,.md,.markdown,.json,.csv"
      onChange={onFilesSelected}
    />
  );
  const disabledBanner = disabledReason ? (
    <div className="border-b border-ui-border px-4 py-2 text-xs leading-5 text-amber-300">
      {disabledReason}
    </div>
  ) : null;
  const attachmentTray = (
    <AttachmentTray
      attachments={attachments}
      uploadingCount={uploadingCount}
      attachmentUrl={attachmentUrl}
      onRemove={(attachmentId) =>
        setAttachments((current) =>
          current.filter((candidate) => candidate.attachmentId !== attachmentId),
        )
      }
    />
  );
  const messageInput = (density: "default" | "compact" = "default") => (
    <>
      <textarea
        ref={textArea}
        aria-activedescendant={activeCommandItem
          ? slashCommandOptionId(commandMenuId, activeCommandItem.value.name)
          : undefined}
        aria-autocomplete={commandMenuOpen ? "list" : undefined}
        aria-controls={commandMenuOpen ? commandMenuId : undefined}
        aria-expanded={commandMenuOpen || undefined}
        aria-haspopup={commandMenuOpen ? "listbox" : undefined}
        aria-label="Message"
        autoFocus={autoFocus}
        className={`w-full resize-none overflow-y-auto bg-transparent px-4 text-base leading-6 text-foreground outline-none placeholder:text-subtle disabled:cursor-not-allowed disabled:opacity-60 ${
          density === "compact" ? "min-h-16 pb-3 pt-3" : "min-h-20 pb-3 pt-3.5"
        }`}
        disabled={Boolean(disabledReason) || sending}
        placeholder="Message your coding agent…"
        role={commandMenuOpen ? "combobox" : undefined}
        rows={density === "compact" ? 2 : 3}
        value={text}
        onBlur={() => setMessageInputFocused(false)}
        onChange={(event) => {
          setText(event.target.value);
          setCommandMenuDismissed(false);
        }}
        onFocus={() => setMessageInputFocused(true)}
        onKeyDown={onKeyDown}
        onPaste={onPaste}
      />
      {commandMenuOpen ? (
        <SlashCommandMenu
          activeCommandName={activeCommandItem?.value.name}
          anchor={textArea}
          id={commandMenuId}
          items={matchingCommandItems}
          onActiveIndexChange={setActiveCommandIndex}
          onExpand={expandSlashCommand}
        />
      ) : null}
    </>
  );
  const attachButton = (kind: "icon" | "text" = "text") => (
    <Button
      size={kind === "icon" ? "icon" : "small"}
      disabled={!attachmentUploader || sending}
      title={attachmentUploader ? "Attach files" : "Attachments are not available in this host"}
      onClick={() => fileInput.current?.click()}
    >
      <Paperclip className="size-3.5" />
      {kind === "text" ? "Add files" : null}
    </Button>
  );
  const interruptButton = (label = false) => onInterrupt ? (
    <Button
      className="rounded-xl"
      size={label ? "default" : "icon"}
      disabled={!interrupting}
      title="Interrupt active turn"
      onClick={() => void onInterrupt().catch(onError)}
    >
      <CircleStop className="size-4" />
      {label ? "Stop" : null}
    </Button>
  ) : null;
  const sendButton = ({
    deliveryMode = mode,
    label = submitLabel,
    tone = "primary",
    arrow = true,
  }: {
    deliveryMode?: chats.ChatPromptMode;
    label?: string;
    tone?: "primary" | "neutral" | "danger";
    arrow?: boolean;
  } = {}) => (
    <Button
      className={`rounded-xl px-3.5 text-sm font-semibold ${sending ? "animate-pulse" : ""}`}
      variant={tone === "primary" ? "primary" : tone === "danger" ? "danger" : "muted"}
      disabled={sendDisabled}
      title={`${label} (Enter)`}
      onClick={() => void submit(deliveryMode)}
    >
      <>{label === "Send" ? null : label}{arrow ? <ArrowUp className="size-3.5" /> : null}</>
    </Button>
  );
  const deliveryControl = (compact = false) => showDeliveryMode ? (
    <SelectControl
      className="text-subtle"
      disabled={sending}
      hideLabel={compact}
      label={compact ? "Send behavior" : "Delivery"}
      options={DELIVERY_OPTIONS}
      value={mode}
      onChange={(nextMode) => setMode(nextMode as chats.ChatPromptMode)}
    />
  ) : null;

  if (layout === "compact") {
    return (
      <div
        className="rounded-2xl border border-ui-border-strong bg-surface shadow-[0_18px_70px_rgb(0_0_0/0.55)] transition focus-within:border-accent/50 focus-within:shadow-[0_18px_70px_rgb(0_0_0/0.65),0_0_0_1px_var(--color-accent-soft)]"
        {...frameEvents}
      >
        {filePicker}
        {disabledBanner}
        {attachmentTray}
        {messageInput("compact")}
        <div className="flex flex-col gap-2 border-t border-ui-border px-3 py-2.5 sm:flex-row sm:items-center sm:justify-between">
          <div className="flex min-w-0 flex-wrap items-center">
            {attachButton("text")}
            <div className="ml-1 border-l border-ui-border pl-1">
              <ModelSettingsMenu
                harness={harness}
                preferences={modelPreferences}
                selection={model}
                disabled={modelLocked || sending}
                onChange={setModel}
              />
            </div>
          </div>
          <div className="flex shrink-0 items-center justify-end gap-2">
            {interruptButton()}
            <div className="flex items-center gap-1 rounded-xl bg-surface-raised/60 p-1">
              {showDeliveryMode ? deliveryControl(true) : null}
              {sendButton()}
            </div>
          </div>
        </div>
      </div>
    );
  }

  if (layout === "structured") {
    return (
      <div
        className="overflow-hidden rounded-2xl border border-ui-border-strong bg-surface shadow-[0_18px_70px_rgb(0_0_0/0.55)] transition focus-within:border-accent/50"
        {...frameEvents}
      >
        {filePicker}
        {disabledBanner}
        <div className="flex items-center gap-2 px-4 pt-3">
          <span className="text-[10px] font-semibold uppercase tracking-[0.14em] text-subtle">
            Message context
          </span>
          {attachButton("text")}
        </div>
        {attachmentTray}
        {messageInput()}
        <div className="grid border-t border-ui-border md:grid-cols-[minmax(0,1fr)_19rem]">
          <section className="p-3" aria-label="Model configuration">
            <p className="mb-2 text-[10px] font-semibold uppercase tracking-[0.14em] text-subtle">
              Model configuration
            </p>
            <ModelControls
              harness={harness}
              preferences={modelPreferences}
              selection={model}
              disabled={modelLocked || sending}
              onChange={setModel}
            />
          </section>
          <section className="border-t border-ui-border p-3 md:border-l md:border-t-0" aria-label="Send behavior">
            <p className="mb-2 text-[10px] font-semibold uppercase tracking-[0.14em] text-subtle">
              Send behavior
            </p>
            <div className="flex items-end justify-between gap-2">
              {deliveryControl()}
              <div className="flex shrink-0 items-center gap-2">
                {interruptButton()}
                {sendButton()}
              </div>
            </div>
          </section>
        </div>
      </div>
    );
  }

  if (layout === "adaptive") {
    return (
      <div
        className="rounded-[1.25rem] border border-ui-border-strong bg-surface shadow-[0_20px_80px_rgb(0_0_0/0.62)] transition focus-within:border-accent/50 focus-within:shadow-[0_20px_80px_rgb(0_0_0/0.7),0_0_0_1px_var(--color-accent-soft)]"
        {...frameEvents}
      >
        {filePicker}
        {disabledBanner}
        <div className="flex flex-wrap items-center justify-between gap-2 px-3 pt-2.5">
          <div className="flex items-center gap-1 text-xs text-subtle">
            <span className="pl-1 font-medium">Context</span>
            {attachButton("text")}
            {attachments.length > 0 ? <span>{attachments.length} added</span> : null}
          </div>
          <ModelSettingsMenu
            align="right"
            harness={harness}
            preferences={modelPreferences}
            selection={model}
            disabled={modelLocked || sending}
            onChange={setModel}
          />
        </div>
        {attachmentTray}
        {messageInput("compact")}
        <div className="flex flex-col gap-2 border-t border-ui-border px-3 py-2.5 sm:flex-row sm:items-center sm:justify-between">
          <div className="flex items-center gap-2 text-xs text-subtle">
            {interrupting ? (
              <>
                <span className="hidden sm:inline">Agent is working</span>
                {interruptButton(true)}
              </>
            ) : (
              <span>Starts a new turn</span>
            )}
          </div>
          {interrupting && showDeliveryMode ? (
            <div className="grid w-full grid-cols-1 gap-2 sm:w-auto sm:grid-cols-3">
              {sendButton({ deliveryMode: "WhenIdle", label: "Queue", arrow: false })}
              {sendButton({ deliveryMode: "Immediate", label: "Steer now", tone: "neutral", arrow: false })}
              {sendButton({ deliveryMode: "InterruptAndSend", label: "Send Now", tone: "danger", arrow: false })}
            </div>
          ) : (
            sendButton({ deliveryMode: "WhenIdle" })
          )}
        </div>
      </div>
    );
  }

  return (
    <>
      <div
        className="rounded-2xl border border-ui-border-strong bg-surface shadow-[0_18px_70px_rgb(0_0_0/0.55)] transition focus-within:border-accent/50 focus-within:shadow-[0_18px_70px_rgb(0_0_0/0.65),0_0_0_1px_var(--color-accent-soft)]"
        {...frameEvents}
      >
        {filePicker}
        {disabledBanner}
        {attachmentTray}
        {messageInput()}

        <div className="flex flex-col gap-3 px-3 py-2.5 sm:flex-row sm:items-center sm:gap-6">
          <div className="flex items-center" role="group" aria-label="Context controls">
            {attachButton("icon")}
          </div>
          <div className="min-w-0 sm:flex-1" role="group" aria-label="Model configuration">
            <ModelControls
              harness={harness}
              preferences={modelPreferences}
              selection={model}
              disabled={modelLocked || sending}
              hideLabels
              onChange={setModel}
            />
          </div>
          <div className="flex shrink-0 items-center justify-end gap-2">
            {interruptButton()}
            {sendButton({ deliveryMode: "WhenIdle" })}
          </div>
        </div>
      </div>

      <div className="flex flex-wrap items-center justify-between gap-x-4 gap-y-1 px-3 pt-2 text-[10px] text-subtle">
        {harness && harnessProvider ? (
          <span className="flex shrink-0 items-center">
            <a
              className="flex items-center gap-1.5 transition-colors hover:text-muted hover:underline hover:underline-offset-2"
              href={harnessProvider.href}
              rel="noreferrer"
              target="_blank"
              title={`Open ${harnessProvider.label} documentation`}
            >
              <HarnessIcon className="size-3" kind={harness.kind} />
              <span>{harnessProvider.label}</span>
            </a>
            {harnessVersion ? (
              <span className="ml-1 font-mono text-subtle">({harnessVersion})</span>
            ) : null}
          </span>
        ) : harness ? (
          <span className="shrink-0">
            {harness.displayName}
            {harnessVersion ? (
              <span className="ml-1 font-mono text-subtle">({harnessVersion})</span>
            ) : null}
          </span>
        ) : null}
        <div className="ml-auto flex flex-wrap items-center justify-end gap-x-4 gap-y-1">
          <Shortcut keys={["Enter"]} label={showDeliveryMode ? "Queue" : submitLabel} />
          <Shortcut keys={["Shift", "Enter"]} label="New line" />
          {showDeliveryMode ? (
            <>
              <Shortcut keys={["Shift", "Mod", "Enter"]} label="Steer" />
              <Shortcut
                keys={["Alt", "Mod", "Enter"]}
                label="Send Now"
              />
            </>
          ) : null}
          {onInterrupt ? <Shortcut keys={["Esc"]} label="Interrupt" /> : null}
        </div>
      </div>
    </>
  );
}

function AttachmentTray({
  attachments,
  uploadingCount,
  attachmentUrl,
  onRemove,
}: {
  attachments: chats.ChatPromptAttachment[];
  uploadingCount: number;
  attachmentUrl?: AttachmentUrlResolver;
  onRemove: (attachmentId: chats.ChatAttachmentId) => void;
}) {
  if (attachments.length === 0 && uploadingCount === 0) return null;
  return (
    <div className="flex flex-wrap gap-2 px-3 pt-3">
      {attachments.map((attachment) => (
        <AttachmentPreview
          attachment={{
            attachmentId: attachment.attachmentId,
            name: attachment.name,
            mediaType: attachment.mediaType,
            size: Number(attachment.size),
          }}
          key={attachment.attachmentId}
          removable
          url={attachmentUrl?.(attachment.attachmentId)}
          onRemove={() => onRemove(attachment.attachmentId)}
        />
      ))}
      {uploadingCount > 0 ? (
        <span className="rounded-lg border border-accent/30 bg-accent/5 px-2.5 py-1.5 text-xs text-accent-text">
          Uploading {uploadingCount} {uploadingCount === 1 ? "attachment" : "attachments"}…
        </span>
      ) : null}
    </div>
  );
}

function ModelSettingsMenu({
  align = "left",
  harness,
  preferences,
  selection,
  disabled,
  onChange,
}: {
  align?: "left" | "right";
  harness?: chats.ChatHarness;
  preferences?: config.WorkspaceChatModelPreferences;
  selection?: chats.ChatModelSelection;
  disabled: boolean;
  onChange: (selection: chats.ChatModelSelection | undefined) => void;
}) {
  const model = harness?.models.find((candidate) => candidate.id === selection?.model) ?? harness?.models[0];
  return (
    <details className="group/model relative">
      <summary className="cursor-pointer list-none rounded-lg px-2.5 py-1.5 text-xs text-muted transition hover:bg-surface-raised hover:text-foreground [&::-webkit-details-marker]:hidden">
        <span className="text-subtle">Model </span>
        {model?.shortName ?? model?.displayName ?? selection?.model ?? "Default"}
      </summary>
      <div className={`absolute bottom-[calc(100%+0.5rem)] z-30 min-w-72 rounded-xl border border-ui-border-strong bg-surface-raised p-3 shadow-2xl shadow-black/60 ${
        align === "right" ? "right-0" : "left-0"
      }`}>
        <p className="mb-3 text-[10px] font-semibold uppercase tracking-[0.14em] text-subtle">
          Model configuration
        </p>
        <ModelControls
          harness={harness}
          preferences={preferences}
          selection={selection}
          disabled={disabled}
          onChange={onChange}
        />
      </div>
    </details>
  );
}

const LARGE_PASTE_CHARACTERS = 4 * 1024;
const TEXTAREA_MAX_HEIGHT = 256;

function timestampForName(): string {
  return new Date().toISOString().replace(/[:.]/g, "-");
}

function looksLikeMarkdown(value: string): boolean {
  return /(^|\n)(#{1,6} |[-*+] |\d+\. |```|> )|\[[^\]]+\]\([^)]+\)/m.test(value);
}

function Shortcut({ keys, label }: { keys: string[]; label: string }) {
  return (
    <span className="inline-flex items-center gap-1.5 whitespace-nowrap">
      <KeyboardShortcut keys={keys} />
      <span className="capitalize">{label}</span>
    </span>
  );
}
