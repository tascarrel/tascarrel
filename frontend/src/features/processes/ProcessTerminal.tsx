import { FitAddon } from "@xterm/addon-fit";
import { SerializeAddon } from "@xterm/addon-serialize";
import { Terminal } from "@xterm/xterm";
import { useEffect, useRef, useState } from "react";

import terminalNerdFontUrl from "@tascarrel/terminal-font";
import { guestApi } from "../../api/client.ts";
import type { processes, workspaces } from "../../api/generated/index.ts";
import { type ProcessTerminalReplica, useProcessTerminal } from "./state.ts";

export function ProcessTerminal({
  workspace,
  process,
}: {
  workspace: workspaces.WorkspaceName;
  process: processes.Process;
}) {
  const terminalState = useProcessTerminal(workspace, process.id);
  const hostRef = useRef<HTMLDivElement>(null);
  const rendererRef = useRef<TerminalRenderer | undefined>(undefined);
  const activeRef = useRef(isActive(process));
  const replayingRef = useRef(true);
  const renderingRef = useRef(true);
  const terminalReadyRef = useRef(false);
  const renderRevisionRef = useRef(0);
  const [controlError, setControlError] = useState<string>();
  const [terminalReady, setTerminalReady] = useState(false);
  const replaying = terminalState.connection !== "live"
    || terminalState.value?.replaying !== false;
  const error = controlError ?? terminalState.error?.message;
  activeRef.current = isActive(process);
  replayingRef.current = replaying;

  useEffect(() => {
    const host = hostRef.current;
    if (!host) return;

    const api = guestApi(workspace);
    const fontReady = loadTerminalFont();
    const checkpoint = terminalState.value?.checkpoint;
    const terminal = new Terminal({
      cols: checkpoint?.cols ?? process.terminal?.cols,
      rows: checkpoint?.rows ?? process.terminal?.rows,
      convertEol: false,
      cursorBlink: true,
      disableStdin: true,
      fontFamily: TERMINAL_FONT_FAMILY,
      fontSize: TERMINAL_FONT_SIZE,
      scrollback: TERMINAL_SCROLLBACK_LINES,
      theme: {
        background: "#000000",
        foreground: "#cbd5e1",
        cursor: "#7dd3fc",
        selectionBackground: "#0ea5e955",
      },
    });
    const fit = new FitAddon();
    const serialize = new SerializeAddon();
    terminal.loadAddon(fit);
    terminal.loadAddon(serialize);

    const renderer: TerminalRenderer = {
      terminal,
      renderedOffset: checkpoint?.offset ?? 0n,
      renderQueue: Promise.resolve(),
      pendingWrites: new Set(),
      disposed: false,
      opened: false,
      syncInteraction: () => undefined,
    };
    rendererRef.current = renderer;
    renderingRef.current = true;
    terminalReadyRef.current = false;
    setTerminalReady(false);
    setControlError(undefined);
    if (checkpoint) {
      terminal.resize(checkpoint.cols, checkpoint.rows);
      renderer.renderQueue = writeTerminal(renderer, checkpoint.serialized)
        .then(() => fontReady)
        .then(() => openTerminal(renderer, host));
    } else {
      renderer.renderQueue = fontReady.then(() => openTerminal(renderer, host));
    }

    let latestSize: { cols: number; rows: number } | undefined;
    let sentSize: string | undefined;
    let resizeTimer: ReturnType<typeof setTimeout> | undefined;
    let inputTimer: ReturnType<typeof setTimeout> | undefined;
    let fitFrame: number | undefined;
    let controlQueue = Promise.resolve();
    let pendingInput: Uint8Array[] = [];
    let pendingInputBytes = 0;
    const abort = new AbortController();

    const reportError = (cause: unknown) => {
      if (!renderer.disposed) setControlError(errorMessage(cause));
    };
    const queueControl = (operation: () => Promise<unknown>) => {
      controlQueue = controlQueue.then(operation).then(() => undefined).catch(reportError);
    };
    const flushInput = () => {
      inputTimer = undefined;
      if (renderer.disposed || pendingInputBytes === 0) return;
      if (replayingRef.current || renderingRef.current || !activeRef.current) {
        pendingInput = [];
        pendingInputBytes = 0;
        return;
      }
      const combined = new Uint8Array(pendingInputBytes);
      let offset = 0;
      for (const bytes of pendingInput) {
        combined.set(bytes, offset);
        offset += bytes.length;
      }
      pendingInput = [];
      pendingInputBytes = 0;
      for (let start = 0; start < combined.length; start += MAX_INPUT_CHUNK_BYTES) {
        const chunk = combined.subarray(start, start + MAX_INPUT_CHUNK_BYTES);
        const data = encodeBase64(chunk);
        queueControl(() => api.execute("processes_WriteTerminal", {
          processId: process.id,
          data: data as processes.ProcessTerminalData,
        }, abort.signal));
      }
    };
    const queueInput = (bytes: Uint8Array) => {
      if (
        renderer.disposed
        || replayingRef.current
        || renderingRef.current
        || !activeRef.current
        || bytes.length === 0
      ) return;
      pendingInput.push(bytes);
      pendingInputBytes += bytes.length;
      if (pendingInputBytes >= MAX_INPUT_CHUNK_BYTES) {
        if (inputTimer !== undefined) clearTimeout(inputTimer);
        flushInput();
      } else if (inputTimer === undefined) {
        inputTimer = setTimeout(flushInput, INPUT_BATCH_DELAY_MS);
      }
    };
    const sendLatestSize = () => {
      if (
        renderer.disposed
        || replayingRef.current
        || renderingRef.current
        || !activeRef.current
        || !latestSize
      ) return;
      const key = `${latestSize.cols}x${latestSize.rows}`;
      if (key === sentSize) return;
      sentSize = key;
      const { cols, rows } = latestSize;
      queueControl(() => api.execute("processes_ResizeTerminal", {
        processId: process.id,
        terminal: {
          cols: cols as processes.ProcessTerminal["cols"],
          rows: rows as processes.ProcessTerminal["rows"],
        },
      }, abort.signal));
    };
    const scheduleResize = () => {
      if (resizeTimer !== undefined) clearTimeout(resizeTimer);
      resizeTimer = setTimeout(sendLatestSize, RESIZE_DELAY_MS);
    };
    const fitTerminal = () => {
      if (fitFrame !== undefined) cancelAnimationFrame(fitFrame);
      fitFrame = requestAnimationFrame(() => {
        fitFrame = undefined;
        if (!renderer.disposed && !replayingRef.current && !renderingRef.current) fit.fit();
      });
    };
    renderer.syncInteraction = () => {
      const inputDisabled = replayingRef.current || renderingRef.current || !activeRef.current;
      terminal.options.disableStdin = inputDisabled;
      if (replayingRef.current || renderingRef.current) return;
      fitTerminal();
      sendLatestSize();
      if (activeRef.current) terminal.focus();
    };

    const dataSubscription = terminal.onData((data) => queueInput(new TextEncoder().encode(data)));
    const binarySubscription = terminal.onBinary((data) => {
      queueInput(Uint8Array.from(data, (character) => character.charCodeAt(0) & 0xff));
    });
    const resizeSubscription = terminal.onResize((size) => {
      latestSize = size;
      scheduleResize();
    });
    const observer = new ResizeObserver(fitTerminal);
    observer.observe(host);

    return () => {
      if (renderer.opened && renderer.pendingWrites.size === 0) {
        try {
          terminalState.cacheCheckpoint({
            offset: renderer.renderedOffset,
            serialized: serialize.serialize({ scrollback: TERMINAL_SCROLLBACK_LINES }),
            cols: terminal.cols,
            rows: terminal.rows,
          });
        } catch (cause) {
          console.error("Failed to cache the process terminal framebuffer", cause);
        }
      }
      renderer.disposed = true;
      renderingRef.current = true;
      terminalReadyRef.current = false;
      renderRevisionRef.current += 1;
      abort.abort();
      observer.disconnect();
      dataSubscription.dispose();
      binarySubscription.dispose();
      resizeSubscription.dispose();
      if (resizeTimer !== undefined) clearTimeout(resizeTimer);
      if (inputTimer !== undefined) clearTimeout(inputTimer);
      if (fitFrame !== undefined) cancelAnimationFrame(fitFrame);
      for (const finishWrite of renderer.pendingWrites) finishWrite();
      if (rendererRef.current === renderer) rendererRef.current = undefined;
      terminal.dispose();
    };
  }, [process.id, terminalState.cacheCheckpoint, workspace]);

  useEffect(() => {
    const renderer = rendererRef.current;
    if (!renderer) return;

    const revision = ++renderRevisionRef.current;
    const restoring = !terminalReadyRef.current || replayingRef.current;
    if (restoring) {
      renderingRef.current = true;
      renderer.terminal.options.disableStdin = true;
      terminalReadyRef.current = false;
      setTerminalReady(false);
    }
    renderer.renderQueue = renderer.renderQueue
      .then(async () => {
        if (renderer.disposed) return;
        if (terminalState.value) await renderReplica(renderer, terminalState.value);
        if (renderer.disposed || revision !== renderRevisionRef.current) return;
        renderingRef.current = false;
        renderer.syncInteraction();
        if (!replayingRef.current) {
          terminalReadyRef.current = true;
          setTerminalReady(true);
        }
      })
      .catch((cause) => {
        if (!renderer.disposed) setControlError(errorMessage(cause));
      });
  }, [process.status.status, replaying, terminalState.value]);

  return (
    <div className="flex h-full min-h-0 flex-1 flex-col">
      {terminalState.value?.hasGap ? (
        <p className="border-b border-amber-500/20 bg-amber-500/5 px-4 py-1.5 text-[10px] text-amber-200">
          Earlier terminal output was evicted; rendering resumed at the oldest retained byte.
        </p>
      ) : null}
      {error ? (
        <p className="border-b border-red-500/20 bg-red-500/5 px-4 py-1.5 text-[10px] text-red-200" role="alert">
          {error}
        </p>
      ) : null}
      <div className="relative min-h-0 flex-1 bg-black p-2">
        <div
          ref={hostRef}
          className={`h-full w-full ${terminalReady ? "visible" : "invisible"}`}
          aria-busy={!terminalReady}
          aria-label={`Process terminal for ${process.title}`}
        />
        {!terminalReady ? <div className="pointer-events-none absolute inset-0 bg-black" aria-hidden="true" /> : null}
      </div>
    </div>
  );
}

async function renderReplica(
  renderer: TerminalRenderer,
  replica: ProcessTerminalReplica,
): Promise<void> {
  const firstOffset = replica.chunks.at(0)?.startOffset ?? replica.nextOffset;
  if (renderer.renderedOffset < firstOffset || renderer.renderedOffset > replica.nextOffset) {
    renderer.terminal.reset();
    renderer.renderedOffset = firstOffset;
  }

  for (const chunk of replica.chunks) {
    if (renderer.disposed) return;
    if (chunk.endOffset <= renderer.renderedOffset) continue;
    if (chunk.startOffset > renderer.renderedOffset) {
      renderer.terminal.reset();
      renderer.renderedOffset = chunk.startOffset;
    }
    const overlap = Number(renderer.renderedOffset - chunk.startOffset);
    await writeTerminal(renderer, chunk.data.subarray(overlap));
    renderer.renderedOffset = chunk.endOffset;
  }

  if (renderer.renderedOffset < replica.nextOffset) {
    renderer.terminal.reset();
    renderer.renderedOffset = replica.nextOffset;
  }
}

function writeTerminal(renderer: TerminalRenderer, data: Uint8Array | string): Promise<void> {
  return new Promise((resolve) => {
    if (renderer.disposed || data.length === 0) {
      resolve();
      return;
    }
    const done = () => {
      renderer.pendingWrites.delete(done);
      resolve();
    };
    renderer.pendingWrites.add(done);
    renderer.terminal.write(data, done);
  });
}

function openTerminal(renderer: TerminalRenderer, host: HTMLElement) {
  if (renderer.disposed) return;
  renderer.terminal.open(host);
  renderer.opened = true;
}

function loadTerminalFont(): Promise<void> {
  if (terminalFontReady) return terminalFontReady;

  const terminalNerdFont = new FontFace(
    "Tascarrel Nerd Font",
    `url(${terminalNerdFontUrl}) format("woff2")`,
  );
  document.fonts.add(terminalNerdFont);
  terminalFontReady = terminalNerdFont.load()
    .then(() => undefined)
    .catch((cause) => {
      console.error("Failed to load the terminal Nerd Font", cause);
    });
  return terminalFontReady;
}

function isActive(process: processes.Process): boolean {
  return process.status.status === "Starting" || process.status.status === "Running";
}

function encodeBase64(data: Uint8Array): string {
  let binary = "";
  for (let start = 0; start < data.length; start += BASE64_ENCODING_CHUNK_BYTES) {
    binary += String.fromCharCode(...data.subarray(start, start + BASE64_ENCODING_CHUNK_BYTES));
  }
  return btoa(binary);
}

function errorMessage(cause: unknown): string {
  return cause instanceof Error ? cause.message : String(cause);
}

type TerminalRenderer = {
  terminal: Terminal;
  renderedOffset: bigint;
  renderQueue: Promise<void>;
  pendingWrites: Set<() => void>;
  disposed: boolean;
  opened: boolean;
  syncInteraction: () => void;
};

const MAX_INPUT_CHUNK_BYTES = 64 * 1024;
const BASE64_ENCODING_CHUNK_BYTES = 16 * 1024;
const INPUT_BATCH_DELAY_MS = 8;
const RESIZE_DELAY_MS = 75;
const TERMINAL_SCROLLBACK_LINES = 10_000;
const TERMINAL_FONT_SIZE = 12;
const TERMINAL_FONT_FAMILY = [
  '"Tascarrel Nerd Font"',
  "ui-monospace",
  "SFMono-Regular",
  "Menlo",
  "Monaco",
  "Consolas",
  '"Liberation Mono"',
  "monospace",
].join(", ");
let terminalFontReady: Promise<void> | undefined;
