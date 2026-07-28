import { useEffect, useState, type FormEvent, type ReactNode } from "react";

import type { auth } from "../api/generated/index.ts";
import { Button } from "../components/ui/Button.tsx";
import { TextInput } from "../components/ui/TextInput.tsx";

const DEFAULT_API_ROOT = "/api/v1";
const BRIDGE_CONTEXT_PATH = "/.tascarrel/context";

type AuthBootstrapProps = {
  onAuthenticated: () => Promise<void>;
};

export function AuthBootstrap({ onAuthenticated }: AuthBootstrapProps) {
  const [state, setState] = useState<
    "checking" | "pairing" | "submitting" | "failed" | "route-expired"
  >("checking");
  const [message, setMessage] = useState("");

  useEffect(() => {
    let active = true;
    void initializeApiRoot()
      .then((apiRoot) => {
        if (!active) return false;
        window.__TASCARREL_API_ROOT__ = apiRoot;
        return fetch(`${apiRoot}/auth/session`, {
          credentials: "same-origin",
          cache: "no-store",
        }).then((response) => {
          if (response.status === 204) return true;
          if (response.status === 401) return false;
          throw new Error("Tascarrel could not verify this browser session.");
        });
      })
      .then((authenticated) => {
        if (!active) return;
        if (authenticated) {
          return onAuthenticated();
        } else {
          setState("pairing");
        }
      })
      .catch((cause) => {
        if (!active) return;
        if (cause instanceof RouteAccessExpiredError) {
          setState("route-expired");
          return;
        }
        setMessage(
          cause instanceof Error
            ? cause.message
            : "Tascarrel could not initialize the browser session.",
        );
        setState("failed");
      });
    return () => {
      active = false;
    };
  }, [onAuthenticated]);

  const pair = async (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    const form = new FormData(event.currentTarget);
    const pairingKey = String(form.get("pairingKey") ?? "").trim();
    const label = String(form.get("label") ?? "").trim();
    if (!pairingKey) return;
    setState("submitting");
    setMessage("");
    try {
      const request: auth.PairBrowserRequest = {
        pairingKey,
        ...(label ? { label } : {}),
      };
      const response = await fetch(
        `${window.__TASCARREL_API_ROOT__ ?? DEFAULT_API_ROOT}/auth/pair`,
        {
          method: "POST",
          credentials: "same-origin",
          headers: { "content-type": "application/json" },
          body: JSON.stringify(request),
        },
      );
      if (!response.ok) {
        throw new Error(
          response.status === 401
            ? "That pairing key is invalid, expired, or has already been used."
            : "Tascarrel could not create the browser session.",
        );
      }
      await onAuthenticated();
    } catch (error) {
      setMessage(
        error instanceof Error ? error.message : "Tascarrel could not pair this browser.",
      );
      setState("failed");
    }
  };

  if (state === "checking") {
    return (
      <AuthPanel title="Checking This Browser" live>
        <p className="text-sm leading-6 text-muted">Looking for an active host session…</p>
      </AuthPanel>
    );
  }

  if (state === "route-expired") {
    return (
      <AuthPanel title="Route Access Expired">
        <p className="text-sm leading-6 text-muted">
          Reopen this trusted frontend from the Tascarrel Network view to issue a new ticket.
        </p>
      </AuthPanel>
    );
  }

  return (
    <AuthPanel title="Pair This Browser">
      <p className="text-sm leading-6 text-muted">
        On the Tascarrel host, run <code>tascarrelctl auth pair</code>, then enter the
        single-use key below.
      </p>
      <form className="mt-6 grid gap-4" onSubmit={(event) => void pair(event)}>
        <label className="grid gap-1.5 text-xs font-medium text-foreground">
          Pairing key
          <TextInput
            name="pairingKey"
            type="password"
            autoComplete="one-time-code"
            spellCheck={false}
            required
            autoFocus
          />
        </label>
        <label className="grid gap-1.5 text-xs font-medium text-foreground">
          <span>
            Device label <span className="font-normal text-muted">(optional)</span>
          </span>
          <TextInput
            name="label"
            type="text"
            autoComplete="off"
            maxLength={80}
            placeholder="Work laptop"
          />
        </label>
        {message ? (
          <p className="text-xs leading-5 text-red-200" role="alert">
            {message}
          </p>
        ) : null}
        <Button
          className="mt-1 w-full"
          type="submit"
          variant="primary"
          disabled={state === "submitting"}
        >
          {state === "submitting" ? "Pairing…" : "Pair browser"}
        </Button>
      </form>
      <p className="mt-5 text-xs leading-5 text-muted">
        Pairing keys expire after ten minutes and work once.
      </p>
    </AuthPanel>
  );
}

function AuthPanel({
  children,
  live = false,
  title,
}: {
  children: ReactNode;
  live?: boolean;
  title: string;
}) {
  return (
    <main className="flex min-h-screen items-center bg-surface px-6 py-8 text-foreground">
      <section
        className="mx-auto w-full max-w-lg rounded-xl border border-ui-border bg-surface-raised p-8"
        aria-live={live ? "polite" : undefined}
      >
        <h1 className="mb-3 text-xl font-semibold tracking-tight">{title}</h1>
        {children}
      </section>
    </main>
  );
}

async function initializeApiRoot(): Promise<string> {
  const response = await fetch(BRIDGE_CONTEXT_PATH, {
    credentials: "same-origin",
    cache: "no-store",
    headers: { accept: "application/json" },
  });
  if (response.status === 401) throw new RouteAccessExpiredError();
  if (response.ok && response.headers.get("content-type")?.includes("application/json")) {
    const context = (await response.json()) as auth.BrowserAuthContext;
    if (context.apiRoot.startsWith("/.tascarrel/")) return context.apiRoot.replace(/\/+$/, "");
  }
  return DEFAULT_API_ROOT;
}

class RouteAccessExpiredError extends Error {}
