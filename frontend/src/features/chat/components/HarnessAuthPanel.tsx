import {
  Check,
  Copy,
  Download,
  ExternalLink,
  LoaderCircle,
  LogOut,
  RefreshCw,
} from "lucide-react";
import { useState } from "react";

import type { chats } from "../../../api/generated/index.ts";
import { Button } from "../../../components/ui/Button.tsx";

type HarnessConnectionCardProps = {
  harness: chats.ChatHarness;
  onInstall: (harness: chats.ChatHarnessKind) => Promise<void>;
  onValidate: (harness: chats.ChatHarnessKind) => Promise<void>;
  onStart: (request: chats.ChatHarnessAuthRequest) => Promise<void>;
  onCancel: (harness: chats.ChatHarnessKind) => Promise<void>;
  onLogout: (harness: chats.ChatHarnessKind) => Promise<void>;
  onError: (cause: unknown) => void;
};

export function HarnessConnectionCard({
  harness,
  onInstall,
  onValidate,
  onStart,
  onCancel,
  onLogout,
  onError,
}: {
  harness: chats.ChatHarness;
  onInstall: HarnessConnectionCardProps["onInstall"];
  onValidate: HarnessConnectionCardProps["onValidate"];
  onStart: HarnessConnectionCardProps["onStart"];
  onCancel: HarnessConnectionCardProps["onCancel"];
  onLogout: HarnessConnectionCardProps["onLogout"];
  onError: HarnessConnectionCardProps["onError"];
}) {
  const [token, setToken] = useState("");
  const [busy, setBusy] = useState(false);
  const run = async (operation: () => Promise<void>) => {
    setBusy(true);
    try {
      await operation();
    } catch (cause) {
      onError(cause);
    } finally {
      setBusy(false);
    }
  };
  const installing = harness.installation.status === "Installing";
  const validating = harness.validatingCredentials;
  const working = busy || installing || validating;

  return (
    <section className="rounded-xl border border-ui-border bg-canvas/70 p-3">
      <div className="flex items-start justify-between gap-2">
        <div>
          <h2 className="text-xs font-semibold text-foreground">{harness.displayName}</h2>
          <p className="mt-1 text-[11px] leading-4 text-subtle">{harnessStatus(harness)}</p>
        </div>
        {harness.credentials.state === "Valid" ? (
          <Check className="size-4 text-emerald-400" aria-label="Validated" />
        ) : working ? (
          <LoaderCircle className="size-4 animate-spin text-accent-text" aria-label="Working" />
        ) : null}
      </div>

      {harness.installation.status !== "Installed" ? (
        <Button
          className="mt-3 h-auto rounded-lg px-2.5 py-1.5 text-[11px]"
          size="small"
          variant="primary"
          disabled={working}
          onClick={() => void run(() => onInstall(harness.kind))}
        >
          <Download aria-hidden="true" className="size-3" />
          {installing ? "Installing…" : "Install harness"}
        </Button>
      ) : harness.kind === "Tasci" ? (
        <p className="mt-3 text-[11px] leading-4 text-subtle">
          Endpoints, models, and tokens are managed in Tasci settings.
        </p>
      ) : harness.login.state === "Pending" ? (
        <PendingChallenge
          state={harness.login}
          busy={working}
          onCancel={() => run(() => onCancel(harness.kind))}
        />
      ) : harness.credentials.state === "Missing" || harness.credentials.state === "Invalid" ? (
        harness.kind === "Codex" ? (
          <Button
            className="mt-3 h-auto rounded-lg px-2.5 py-1.5 text-[11px]"
            size="small"
            variant="primary"
            disabled={working}
            onClick={() => void run(() => onStart({ kind: "CodexDeviceCode" }))}
          >
            Sign in with ChatGPT
          </Button>
        ) : (
          <ClaudeTokenForm
            busy={working}
            token={token}
            onTokenChange={setToken}
            onSubmit={() => run(async () => {
              await onStart({ kind: "ClaudeSetupToken", token: token.trim() });
              setToken("");
            })}
          />
        )
      ) : (
        <div className="mt-3 flex flex-wrap gap-3">
          {harness.credentials.state !== "Valid" ? (
            <Button
              className="h-auto border-0 bg-transparent p-0 text-[11px] text-accent-text hover:text-accent"
              size="small"
              disabled={working}
              onClick={() => void run(() => onValidate(harness.kind))}
            >
              <RefreshCw aria-hidden="true" className="size-3" /> Validate
            </Button>
          ) : null}
          <Button
            className="h-auto border-0 bg-transparent p-0 text-[11px] text-subtle hover:text-muted"
            size="small"
            disabled={working}
            onClick={() => void run(() => onLogout(harness.kind))}
          >
            <LogOut aria-hidden="true" className="size-3" /> Sign out
          </Button>
        </div>
      )}

      {harnessError(harness) ? (
        <p className="mt-2 text-[11px] leading-4 text-red-300">{harnessError(harness)}</p>
      ) : null}
    </section>
  );
}

function ClaudeTokenForm({
  busy,
  token,
  onTokenChange,
  onSubmit,
}: {
  busy: boolean;
  token: string;
  onTokenChange: (token: string) => void;
  onSubmit: () => Promise<void>;
}) {
  return (
    <form
      className="mt-3"
      onSubmit={(event) => {
        event.preventDefault();
        if (token.trim()) void onSubmit();
      }}
    >
      <p className="mb-2 text-[11px] leading-4 text-subtle">
        Run <code className="text-muted">claude setup-token</code>, then paste its token.
      </p>
      <div className="flex gap-2">
        <input
          className="min-w-0 flex-1 rounded-lg border border-ui-border bg-surface px-2 py-1.5 text-[11px] text-foreground outline-none focus:border-accent/50"
          aria-label="Claude setup token"
          autoComplete="off"
          placeholder="Setup token"
          type="password"
          value={token}
          onChange={(event) => onTokenChange(event.target.value)}
        />
        <Button
          className="h-auto rounded-lg px-2.5 py-1.5 text-[11px]"
          type="submit"
          size="small"
          variant="primary"
          disabled={busy || !token.trim()}
        >
          Connect
        </Button>
      </div>
    </form>
  );
}

function PendingChallenge({
  state,
  busy,
  onCancel,
}: {
  state: Extract<chats.ChatHarnessLoginState, { state: "Pending" }>;
  busy: boolean;
  onCancel: () => Promise<void>;
}) {
  const url = safeAuthorizationUrl(state.authorizationUrl);
  return (
    <div className="mt-3 space-y-2 text-[11px]">
      {state.userCode ? (
        <Button
          className="h-auto rounded-lg px-2 py-1 font-mono text-foreground"
          size="small"
          title="Copy device code"
          onClick={() => void navigator.clipboard.writeText(state.userCode ?? "")}
        >
          {state.userCode} <Copy aria-hidden="true" className="size-3" />
        </Button>
      ) : null}
      <div className="flex flex-wrap items-center gap-3">
        {url ? (
          <a className="inline-flex items-center gap-1 text-accent-text hover:text-accent" href={url} target="_blank" rel="noreferrer">
            Continue authentication <ExternalLink aria-hidden="true" className="size-3" />
          </a>
        ) : null}
        <Button
          className="h-auto border-0 bg-transparent p-0 text-subtle hover:text-muted"
          size="small"
          disabled={busy}
          onClick={() => void onCancel()}
        >
          Cancel
        </Button>
      </div>
    </div>
  );
}

function harnessStatus(harness: chats.ChatHarness): string {
  if (harness.installation.status === "NotInstalled") return `Not installed · ${harness.pinnedVersion}`;
  if (harness.installation.status === "Installing") return `Installing ${harness.pinnedVersion}`;
  if (harness.installation.status === "Failed") return "Installation failed";
  if (harness.login.state === "Pending") return "Waiting for authorization";
  if (harness.validatingCredentials) return "Validating credentials";
  if (harness.credentials.state === "Valid") {
    return `Connected · ${harness.credentials.email ?? harness.credentials.plan ?? harness.credentials.method}`;
  }
  if (harness.credentials.state === "Present") return "Credentials available · validation required";
  if (harness.credentials.state === "Invalid") return "Credentials are invalid";
  return "Not connected";
}

function harnessError(harness: chats.ChatHarness): string | undefined {
  if (harness.installation.status === "Failed") return harness.installation.message;
  if (harness.login.state === "Failed") return harness.login.message;
  if (harness.credentials.state === "Invalid") return harness.credentials.message;
  return undefined;
}

function safeAuthorizationUrl(value: string): string | undefined {
  try {
    const url = new URL(value);
    return url.protocol === "https:" ? url.toString() : undefined;
  } catch {
    return undefined;
  }
}
