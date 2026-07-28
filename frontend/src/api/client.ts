import type {
  GuestActions,
  GuestSubscriptions,
  HostActions,
  HostSubscriptions,
} from "./actions.ts";
import type { protocol, workspaces } from "./generated/index.ts";
import { AUTH_SESSION_API_PATH, CONTROL_API_PATH } from "./paths.ts";

type OperationInput<O> = O extends { input: infer I } ? I : never;
type OperationOutput<O> = O extends { output: infer O } ? O : never;
type OperationName<R> = Extract<keyof R, string>;
type SubscriptionInput<I> = I | (() => I);

export type SubscriptionState = "connecting" | "live" | "reconnecting";

export type SubscriptionHandlers<E> = {
  onEvent: (event: E) => void | Promise<void>;
  onState?: (state: SubscriptionState, attempt: number) => void;
  onError?: (error: TascarrelApiError) => void;
};

export type SubscriptionOptions = {
  eventCreditWindow?: number;
  reconnectOnComplete?: boolean;
};

export class TascarrelApiError extends Error {
  public readonly operationError: protocol.OperationError;

  constructor(error: protocol.OperationError) {
    super(error.message);
    this.name = "TascarrelApiError";
    this.operationError = error;
  }
}

const HOST_ADDRESS: protocol.Address = { type: "Host" };
const EVENT_CREDIT_WINDOW = 16;
const ID_ALPHABET = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";
const ID_LENGTH = 22;
const HEARTBEAT_INTERVAL_MS = 5_000;

export const hostApi = {
  execute<A extends OperationName<HostActions>>(
    action: A,
    input: OperationInput<HostActions[A]>,
    signal?: AbortSignal,
  ): Promise<OperationOutput<HostActions[A]>> {
    return executeAction<HostActions, A>(HOST_ADDRESS, action, input, signal);
  },

  subscribe<S extends OperationName<HostSubscriptions>>(
    name: S,
    input: SubscriptionInput<OperationInput<HostSubscriptions[S]>>,
    handlers: SubscriptionHandlers<OperationOutput<HostSubscriptions[S]>>,
    options?: SubscriptionOptions,
  ): () => void {
    return subscribe<HostSubscriptions, S>(HOST_ADDRESS, name, input, handlers, options);
  },
};

export function guestApi(workspace: workspaces.WorkspaceName) {
  const target: protocol.Address = { type: "Workspace", workspace };
  return {
    execute<A extends OperationName<GuestActions>>(
      action: A,
      input: OperationInput<GuestActions[A]>,
      signal?: AbortSignal,
    ): Promise<OperationOutput<GuestActions[A]>> {
      return executeAction<GuestActions, A>(target, action, input, signal);
    },

    subscribe<S extends OperationName<GuestSubscriptions>>(
      name: S,
      input: SubscriptionInput<OperationInput<GuestSubscriptions[S]>>,
      handlers: SubscriptionHandlers<OperationOutput<GuestSubscriptions[S]>>,
      options?: SubscriptionOptions,
    ): () => void {
      return subscribe<GuestSubscriptions, S>(target, name, input, handlers, options);
    },
  };
}

async function executeAction<R, A extends OperationName<R>>(
  target: protocol.Address,
  action: A,
  input: OperationInput<R[A]>,
  signal?: AbortSignal,
): Promise<OperationOutput<R[A]>> {
  const id = generateInvocationId();
  const message: protocol.Message = {
    protocol: "Rpc",
    content: {
      type: "Invoke",
      id,
      target,
      procedure: action,
      input,
    },
  };
  return controlConnection.execute<OperationOutput<R[A]>>(id, message, signal);
}

function subscribe<R, S extends OperationName<R>>(
  target: protocol.Address,
  name: S,
  input: SubscriptionInput<OperationInput<R[S]>>,
  handlers: SubscriptionHandlers<OperationOutput<R[S]>>,
  options: SubscriptionOptions = {},
): () => void {
  return controlConnection.subscribe(
    (id) => ({
      protocol: "Subscription",
      content: {
        type: "Subscribe",
        id,
        target,
        subscription: name,
        input: resolveSubscriptionInput(input),
      },
    }),
    (event) => handlers.onEvent(event as OperationOutput<R[S]>),
    handlers.onState,
    handlers.onError,
    options,
  );
}

type ActiveInvocation = {
  id: protocol.InvocationId;
  message: protocol.Message;
  sent: boolean;
  resolve: (output: unknown) => void;
  reject: (cause: unknown) => void;
  signal?: AbortSignal;
  abort: () => void;
};

type ActiveSubscription = {
  active: boolean;
  attempt: number;
  id?: protocol.SubscriptionId;
  eventQueue: Promise<void>;
  restart?: ReturnType<typeof setTimeout>;
  createMessage: (id: protocol.SubscriptionId) => protocol.Message;
  onEvent: (event: unknown) => void | Promise<void>;
  onState?: SubscriptionHandlers<unknown>["onState"];
  onError?: SubscriptionHandlers<unknown>["onError"];
  options: SubscriptionOptions;
};

/** Multiplexes every frontend RPC and subscription over one control-plane socket. */
class ControlConnection {
  private socket: WebSocket | undefined;
  private retry: ReturnType<typeof setTimeout> | undefined;
  private heartbeat: ReturnType<typeof setInterval> | undefined;
  private reconnectAttempt = 0;
  private readonly invocations = new Map<protocol.InvocationId, ActiveInvocation>();
  private readonly subscriptions = new Set<ActiveSubscription>();
  private readonly subscriptionsById = new Map<protocol.SubscriptionId, ActiveSubscription>();

  public execute<O>(
    id: protocol.InvocationId,
    message: protocol.Message,
    signal?: AbortSignal,
  ): Promise<O> {
    return new Promise((resolve, reject) => {
      const abort = () => {
        const invocation = this.invocations.get(id);
        if (!invocation) return;
        if (invocation.sent) {
          this.send({
            protocol: "Rpc",
            content: { type: "Cancel", id },
          });
        }
        this.finishInvocation(invocation, () => {
          reject(new DOMException("The operation was aborted", "AbortError"));
        });
      };
      const invocation: ActiveInvocation = {
        id,
        message,
        sent: false,
        resolve: (output) => resolve(output as O),
        reject,
        signal,
        abort,
      };
      this.invocations.set(id, invocation);
      signal?.addEventListener("abort", abort, { once: true });
      if (signal?.aborted) {
        abort();
        return;
      }
      this.ensureConnected();
      this.sendInvocation(invocation);
    });
  }

  public subscribe(
    createMessage: (id: protocol.SubscriptionId) => protocol.Message,
    onEvent: (event: unknown) => void | Promise<void>,
    onState: SubscriptionHandlers<unknown>["onState"],
    onError: SubscriptionHandlers<unknown>["onError"],
    options: SubscriptionOptions,
  ): () => void {
    const subscription: ActiveSubscription = {
      active: true,
      attempt: 0,
      eventQueue: Promise.resolve(),
      createMessage,
      onEvent,
      onState,
      onError,
      options,
    };
    this.subscriptions.add(subscription);
    if (this.socket?.readyState === WebSocket.OPEN) {
      this.startSubscription(subscription);
    } else {
      if (this.socket || this.retry !== undefined) this.reportConnecting(subscription);
      this.ensureConnected();
    }
    return () => this.stopSubscription(subscription, true);
  }

  private ensureConnected(): void {
    if (this.socket || this.retry !== undefined) return;
    this.connect(true);
  }

  private connect(reportState: boolean): void {
    if (reportState) {
      for (const subscription of this.subscriptions) this.reportConnecting(subscription);
    }
    let connection: WebSocket;
    try {
      connection = new WebSocket(websocketUrl(CONTROL_API_PATH));
    } catch (cause) {
      const error = unavailable(
        `Failed to open the Tascarrel control-plane connection: ${errorMessage(cause)}`,
      );
      this.failInvocations(error);
      this.scheduleReconnect();
      return;
    }
    this.socket = connection;
    connection.addEventListener("open", () => this.handleOpen(connection));
    connection.addEventListener("message", (raw) => this.handleMessage(connection, raw.data));
    connection.addEventListener("error", () => this.handleError(connection));
    connection.addEventListener("close", () => this.handleClose(connection));
  }

  private handleOpen(connection: WebSocket): void {
    if (this.socket !== connection) return;
    this.reconnectAttempt = 0;
    this.heartbeat = setInterval(() => {
      this.send({
        protocol: "Control",
        content: { type: "Ping" },
      });
    }, HEARTBEAT_INTERVAL_MS);
    for (const invocation of this.invocations.values()) this.sendInvocation(invocation);
    for (const subscription of this.subscriptions) {
      void subscription.eventQueue.then(() => {
        if (this.socket === connection) this.startSubscription(subscription);
      });
    }
  }

  private handleMessage(connection: WebSocket, data: unknown): void {
    if (this.socket !== connection) return;
    const message = parseMessage(data);
    if (message?.protocol === "Rpc") {
      this.handleRpc(message.content);
    } else if (message?.protocol === "Subscription") {
      this.handleSubscription(message.content);
    }
  }

  private handleRpc(message: protocol.RpcMessage): void {
    const invocation = this.invocations.get(message.id);
    if (!invocation) return;
    if (message.type === "Completed") {
      this.finishInvocation(invocation, () => invocation.resolve(message.output));
    } else if (message.type === "Failed") {
      this.finishInvocation(invocation, () => {
        invocation.reject(new TascarrelApiError(message.error));
      });
    } else if (message.type === "Canceled") {
      this.finishInvocation(invocation, () => {
        invocation.reject(new DOMException("The operation was canceled", "AbortError"));
      });
    }
  }

  private handleSubscription(message: protocol.SubscriptionMessage): void {
    const subscription = this.subscriptionsById.get(message.id);
    if (!subscription) return;
    if (message.type === "Event") {
      const id = message.id;
      const event = message.event;
      subscription.eventQueue = subscription.eventQueue
        .then(() => subscription.onEvent(event))
        .then(() => {
          if (subscription.active && subscription.id === id) this.grantCredit(id, 1);
        })
        .catch((cause: unknown) => {
          if (!subscription.active) return;
          subscription.onError?.(unavailable(
            `The subscription event handler failed: ${errorMessage(cause)}`,
          ));
          this.stopSubscription(subscription, true);
        });
    } else if (message.type === "Failed") {
      subscription.onError?.(new TascarrelApiError(message.error));
      this.stopSubscription(subscription, false);
    } else if (message.type === "Completed") {
      this.subscriptionsById.delete(message.id);
      subscription.id = undefined;
      if (subscription.options.reconnectOnComplete ?? false) {
        this.scheduleSubscriptionRestart(subscription);
      } else {
        this.stopSubscription(subscription, false);
      }
    }
  }

  private handleError(connection: WebSocket): void {
    if (this.socket !== connection) return;
    const error = unavailable("The Tascarrel control-plane connection failed");
    this.failInvocations(error);
    for (const subscription of this.subscriptions) this.reportConnecting(subscription);
  }

  private handleClose(connection: WebSocket): void {
    if (this.socket !== connection) return;
    this.socket = undefined;
    if (this.heartbeat !== undefined) clearInterval(this.heartbeat);
    this.heartbeat = undefined;
    this.subscriptionsById.clear();
    for (const subscription of this.subscriptions) {
      subscription.id = undefined;
      if (subscription.restart !== undefined) clearTimeout(subscription.restart);
      subscription.restart = undefined;
    }
    this.failInvocations(unavailable("The Tascarrel control-plane connection closed"));
    void fetch(AUTH_SESSION_API_PATH, {
      credentials: "same-origin",
      cache: "no-store",
    }).then((response) => {
      if (response.status === 401) window.location.reload();
    }).catch((cause) => {
      console.debug("Could not check whether the browser session remains active", cause);
    });
    this.scheduleReconnect();
  }

  private scheduleReconnect(): void {
    if (this.socket || this.retry !== undefined || this.subscriptions.size === 0) return;
    this.reconnectAttempt += 1;
    for (const subscription of this.subscriptions) this.reportConnecting(subscription);
    const delay = Math.min(
      500 * 2 ** Math.min(this.reconnectAttempt - 1, 5),
      10_000,
    );
    this.retry = setTimeout(() => {
      this.retry = undefined;
      if (this.subscriptions.size > 0) this.connect(false);
    }, delay);
  }

  private sendInvocation(invocation: ActiveInvocation): void {
    if (invocation.sent || this.invocations.get(invocation.id) !== invocation) return;
    invocation.sent = this.send(invocation.message);
  }

  private finishInvocation(invocation: ActiveInvocation, callback: () => void): void {
    if (this.invocations.get(invocation.id) !== invocation) return;
    this.invocations.delete(invocation.id);
    invocation.signal?.removeEventListener("abort", invocation.abort);
    callback();
  }

  private failInvocations(error: TascarrelApiError): void {
    for (const invocation of [...this.invocations.values()]) {
      this.finishInvocation(invocation, () => invocation.reject(error));
    }
  }

  private startSubscription(subscription: ActiveSubscription): void {
    if (
      !subscription.active
      || subscription.id !== undefined
      || subscription.restart !== undefined
      || this.socket?.readyState !== WebSocket.OPEN
    ) return;
    const id = generateSubscriptionId();
    subscription.id = id;
    subscription.attempt += 1;
    this.subscriptionsById.set(id, subscription);
    subscription.onState?.("live", subscription.attempt);
    if (!this.send(subscription.createMessage(id))) return;
    this.grantCredit(id, subscription.options.eventCreditWindow ?? EVENT_CREDIT_WINDOW);
  }

  private stopSubscription(subscription: ActiveSubscription, sendUnsubscribe: boolean): void {
    if (!subscription.active) return;
    subscription.active = false;
    this.subscriptions.delete(subscription);
    if (subscription.restart !== undefined) clearTimeout(subscription.restart);
    subscription.restart = undefined;
    const id = subscription.id;
    subscription.id = undefined;
    if (id === undefined) return;
    this.subscriptionsById.delete(id);
    if (sendUnsubscribe) {
      this.send({
        protocol: "Subscription",
        content: { type: "Unsubscribe", id },
      });
    }
  }

  private scheduleSubscriptionRestart(subscription: ActiveSubscription): void {
    if (!subscription.active || subscription.restart !== undefined) return;
    this.reportConnecting(subscription);
    const delay = Math.min(
      500 * 2 ** Math.min(Math.max(subscription.attempt - 1, 0), 5),
      10_000,
    );
    subscription.restart = setTimeout(() => {
      subscription.restart = undefined;
      if (!subscription.active) return;
      if (this.socket?.readyState === WebSocket.OPEN) {
        void subscription.eventQueue.then(() => this.startSubscription(subscription));
      } else {
        this.ensureConnected();
      }
    }, delay);
  }

  private reportConnecting(subscription: ActiveSubscription): void {
    subscription.onState?.(
      subscription.attempt === 0 ? "connecting" : "reconnecting",
      subscription.attempt + 1,
    );
  }

  private grantCredit(id: protocol.SubscriptionId, events: number): void {
    this.send({
      protocol: "Subscription",
      content: {
        type: "GrantCredit",
        id,
        events: events as protocol.SubscriptionCredit["events"],
      },
    });
  }

  private send(message: protocol.Message): boolean {
    if (this.socket?.readyState !== WebSocket.OPEN) return false;
    this.socket.send(JSON.stringify(message));
    return true;
  }
}

const controlConnection = new ControlConnection();

function resolveSubscriptionInput<I>(input: SubscriptionInput<I>): I {
  return typeof input === "function" ? (input as () => I)() : input;
}

function parseMessage(data: unknown): protocol.Message | undefined {
  try {
    return JSON.parse(String(data)) as protocol.Message;
  } catch {
    return undefined;
  }
}

function unavailable(message: string): TascarrelApiError {
  return new TascarrelApiError({ type: "Unavailable", message });
}

function errorMessage(cause: unknown): string {
  return cause instanceof Error ? cause.message : String(cause);
}

function generateSubscriptionId(): protocol.SubscriptionId {
  return `subscription_${randomIdSuffix()}` as protocol.SubscriptionId;
}

function generateInvocationId(): protocol.InvocationId {
  return `invocation_${randomIdSuffix()}` as protocol.InvocationId;
}

function randomIdSuffix(): string {
  const random = crypto.getRandomValues(new Uint8Array(ID_LENGTH));
  return Array.from(random, (byte) => ID_ALPHABET[byte % ID_ALPHABET.length]).join("");
}

function websocketUrl(path: string): string {
  const url = new URL(path, window.location.href);
  url.protocol = url.protocol === "https:" ? "wss:" : "ws:";
  return url.toString();
}
