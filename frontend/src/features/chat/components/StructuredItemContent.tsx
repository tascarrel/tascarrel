import { memo } from "react";

import type { chats } from "../../../api/generated/index.ts";
import { prettyJson } from "../model/format.ts";
import { HighlightedCode, MarkdownContent } from "./MarkdownContent.tsx";

export const StructuredItemContent = memo(function StructuredItemContent({
  kind,
  value,
}: {
  kind: chats.ChatItemKind;
  value: unknown;
}) {
  const record = asRecord(value);
  if (!record) return <HighlightedCode code={prettyJson(value)} language="json" />;

  switch (kind) {
    case "CommandExecution":
      return <CommandContent value={record} />;
    case "FileChange":
      return <FileChangeContent value={record} />;
    case "ToolCall":
      return <ToolCallContent value={record} />;
    case "WebSearch":
      return <WebSearchContent value={record} />;
    case "Subagent":
      return <SubagentContent value={record} />;
    case "Error":
      return <MarkdownContent content={stringValue(record, "message", "error") ?? prettyJson(value)} />;
    default:
      return <HighlightedCode code={prettyJson(value)} language="json" />;
  }
});

function CommandContent({ value }: { value: Record<string, unknown> }) {
  const command = stringValue(value, "command", "cmd") ?? "Command";
  const output = stringValue(value, "aggregatedOutput", "output", "stdout", "stderr");
  const cwd = stringValue(value, "cwd", "workingDirectory");
  const exitCode = scalarValue(value.exitCode);
  return (
    <div className="space-y-3">
      {cwd || exitCode !== undefined ? (
        <div className="flex flex-wrap items-center gap-2 text-xs text-muted">
          {cwd ? <span className="font-mono text-[10px] text-subtle">{cwd}</span> : null}
          {exitCode !== undefined ? (
            <span className={`ml-auto rounded-full px-2 py-0.5 text-[10px] ${String(exitCode) === "0" ? "bg-emerald-500/10 text-emerald-300" : "bg-red-500/10 text-red-300"}`}>
              exit {exitCode}
            </span>
          ) : null}
        </div>
      ) : null}
      <HighlightedCode code={command} language="bash" />
      {output ? <HighlightedCode code={output} language="text" /> : null}
    </div>
  );
}

function FileChangeContent({ value }: { value: Record<string, unknown> }) {
  const changes = Array.isArray(value.changes) ? value.changes : [];
  if (!changes.length) return <HighlightedCode code={prettyJson(value)} language="json" />;
  return (
    <div className="divide-y divide-ui-border overflow-hidden rounded-xl border border-ui-border bg-surface-raised">
      {changes.map((change, index) => {
        const record = asRecord(change);
        const path = record ? stringValue(record, "path", "filePath", "file") : undefined;
        const operation = record ? stringValue(record, "kind", "type", "operation") : undefined;
        return (
          <div className="flex items-center gap-2.5 px-3 py-2 text-xs" key={`${path ?? "change"}-${index}`}>
            <span className="min-w-0 flex-1 truncate font-mono text-muted">{path ?? prettyJson(change)}</span>
            {operation ? <span className="text-[10px] uppercase text-subtle">{operation}</span> : null}
          </div>
        );
      })}
    </div>
  );
}

function ToolCallContent({ value }: { value: Record<string, unknown> }) {
  const server = stringValue(value, "server", "serverName");
  const tool = stringValue(value, "tool", "toolName", "name") ?? "Tool call";
  const input = value.arguments ?? value.input ?? value.params;
  const result = value.result ?? value.output;
  return (
    <div className="space-y-3">
      <div className="flex items-center gap-2 text-xs text-muted">
        <span className="font-medium text-foreground">{tool}</span>
        {server ? <span className="font-mono text-[10px] text-subtle">via {server}</span> : null}
      </div>
      {input !== undefined ? <LabeledJson label="Input" value={input} /> : null}
      {result !== undefined ? <LabeledJson label="Result" value={result} /> : null}
    </div>
  );
}

function WebSearchContent({ value }: { value: Record<string, unknown> }) {
  const query = stringValue(value, "query", "text") ?? "Web search";
  return (
    <div className="rounded-xl border border-ui-border bg-surface-raised px-3 py-2.5 text-sm leading-6 text-foreground">
      {query}
    </div>
  );
}

function SubagentContent({ value }: { value: Record<string, unknown> }) {
  const prompt = stringValue(value, "prompt", "message");
  const tool = stringValue(value, "tool", "name") ?? "Subagent";
  return (
    <div className="space-y-2 rounded-xl border border-ui-border bg-surface-raised px-3 py-2.5">
      <div className="text-xs font-medium text-foreground">{tool}</div>
      {prompt ? <MarkdownContent content={prompt} /> : <HighlightedCode code={prettyJson(value)} language="json" />}
    </div>
  );
}

function LabeledJson({ label, value }: { label: string; value: unknown }) {
  return (
    <details className="group rounded-xl border border-ui-border bg-surface-raised" open={label === "Result"}>
      <summary className="cursor-pointer px-3 py-2 text-xs text-muted">{label}</summary>
      <div className="border-t border-ui-border px-3 py-1">
        <HighlightedCode code={prettyJson(value)} language="json" />
      </div>
    </details>
  );
}

function asRecord(value: unknown): Record<string, unknown> | undefined {
  return value && typeof value === "object" && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : undefined;
}

function stringValue(record: Record<string, unknown>, ...keys: string[]): string | undefined {
  for (const key of keys) {
    const value = record[key];
    if (typeof value === "string") return value;
    if (key === "error" && value && typeof value === "object") {
      const message = (value as Record<string, unknown>).message;
      if (typeof message === "string") return message;
    }
  }
  return undefined;
}

function scalarValue(value: unknown): string | number | boolean | undefined {
  return typeof value === "string" || typeof value === "number" || typeof value === "boolean"
    ? value
    : undefined;
}
