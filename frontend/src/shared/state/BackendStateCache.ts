/**
 * Shares one resumable backend subscription for each explicitly keyed state replica.
 * Dormant LRU resources retain their latest snapshot and cursor until eviction.
 */
export class BackendStateCache {
  private readonly entries = new Map<string, BackendStateEntry<unknown, unknown, unknown>>();

  public constructor(private readonly maximumDormantEntries = 16) {}

  public resource<T, E, C>(
    definition: BackendStateDefinition<T, E, C>,
  ): BackendStateResource<T> {
    const current = this.entries.get(definition.key);
    if (current) return current as BackendStateEntry<T, E, C>;

    const entry = new BackendStateEntry(definition, () => this.evictDormantEntries());
    this.entries.set(definition.key, entry as BackendStateEntry<unknown, unknown, unknown>);
    return entry;
  }

  public evict(key: string): void {
    const entry = this.entries.get(key);
    if (!entry || entry.active) return;
    entry.dispose();
    this.entries.delete(key);
  }

  private evictDormantEntries(): void {
    const dormant = [...this.entries.values()]
      .filter((entry) => !entry.active && entry.retention === "lru")
      .sort((left, right) => left.lastObservedAt - right.lastObservedAt);
    while (dormant.length > this.maximumDormantEntries) {
      const entry = dormant.shift();
      if (!entry) break;
      entry.dispose();
      this.entries.delete(entry.key);
    }
  }
}

export type BackendConnectionState = "idle" | "connecting" | "live" | "reconnecting";

export type BackendStateSnapshot<T> = Readonly<{
  value?: T;
  ready: boolean;
  connection: BackendConnectionState;
  connectionAttempt: number;
  error?: Error;
}>;

export type BackendStateEventResult<T, C> = {
  value: T;
  cursor?: C;
};

export type BackendStateDefinition<T, E, C> = {
  key: string;
  retention?: "persistent" | "lru";
  connect: (
    cursor: () => C | undefined,
    handlers: {
      onEvent: (event: E) => void;
      onConnection: (state: Exclude<BackendConnectionState, "idle">, attempt: number) => void;
      onError: (error: Error) => void;
    },
  ) => () => void;
  applyEvent: (current: T | undefined, event: E) => BackendStateEventResult<T, C>;
};

export type BackendStateResource<T> = {
  getSnapshot: () => BackendStateSnapshot<T>;
  subscribe: (listener: Listener) => () => void;
  refresh: () => void;
  /** Updates derived frontend state without changing the backend subscription cursor. */
  updateValue: (updater: (current: T | undefined) => T | undefined) => void;
};

type Listener = () => void;

const INITIAL_SNAPSHOT: BackendStateSnapshot<never> = {
  ready: false,
  connection: "idle",
  connectionAttempt: 0,
};

/** Owns the shared snapshot, cursor, and physical subscription for one cache key. */
class BackendStateEntry<T, E, C> implements BackendStateResource<T> {
  private snapshot = INITIAL_SNAPSHOT as BackendStateSnapshot<T>;
  private cursor: C | undefined;
  private readonly listeners = new Set<Listener>();
  private disconnect: (() => void) | undefined;
  private closeRevision = 0;
  private frame: number | undefined;
  public lastObservedAt = Date.now();

  public constructor(
    private readonly definition: BackendStateDefinition<T, E, C>,
    private readonly onDormant: () => void,
  ) {}

  public get key(): string {
    return this.definition.key;
  }

  public get retention(): "persistent" | "lru" {
    return this.definition.retention ?? "persistent";
  }

  public get active(): boolean {
    return this.listeners.size > 0;
  }

  public getSnapshot = (): BackendStateSnapshot<T> => this.snapshot;

  public subscribe = (listener: Listener): (() => void) => {
    this.closeRevision += 1;
    this.lastObservedAt = Date.now();
    this.listeners.add(listener);
    if (this.listeners.size === 1) this.start();
    return () => {
      this.listeners.delete(listener);
      if (this.listeners.size > 0) return;
      const revision = ++this.closeRevision;
      queueMicrotask(() => {
        if (revision !== this.closeRevision || this.listeners.size > 0) return;
        this.stop();
        this.lastObservedAt = Date.now();
        this.onDormant();
      });
    };
  };

  public refresh = (): void => {
    if (!this.active) return;
    this.stop();
    this.start();
  };

  public updateValue = (updater: (current: T | undefined) => T | undefined): void => {
    try {
      const value = updater(this.snapshot.value);
      if (value === undefined || Object.is(value, this.snapshot.value)) return;
      this.publish({ ...this.snapshot, value, ready: true }, false);
    } catch (cause) {
      this.publish({
        ...this.snapshot,
        error: cause instanceof Error ? cause : new Error(String(cause)),
      }, false);
    }
  };

  public dispose(): void {
    this.stop();
    if (this.frame !== undefined) window.cancelAnimationFrame(this.frame);
    this.frame = undefined;
    this.listeners.clear();
  }

  private start(): void {
    if (this.disconnect) return;
    this.publish({ ...this.snapshot, connection: "connecting", connectionAttempt: 1 }, false);
    this.disconnect = this.definition.connect(
      () => this.cursor,
      {
        onEvent: (event) => {
          try {
            const next = this.definition.applyEvent(this.snapshot.value, event);
            this.cursor = next.cursor;
            this.publish({
              value: next.value,
              ready: true,
              connection: this.snapshot.connection === "idle" ? "connecting" : this.snapshot.connection,
              connectionAttempt: this.snapshot.connectionAttempt || 1,
            }, true);
          } catch (cause) {
            this.publish({
              ...this.snapshot,
              error: cause instanceof Error ? cause : new Error(String(cause)),
            }, false);
          }
        },
        onConnection: (connection, attempt) => this.publish({
          ...this.snapshot,
          connection,
          connectionAttempt: attempt,
        }, false),
        onError: (error) => this.publish({ ...this.snapshot, error }, false),
      },
    );
  }

  private stop(): void {
    this.disconnect?.();
    this.disconnect = undefined;
    this.snapshot = { ...this.snapshot, connection: "idle", connectionAttempt: 0 };
  }

  private publish(snapshot: BackendStateSnapshot<T>, coalesce: boolean): void {
    this.snapshot = snapshot;
    if (!coalesce) {
      this.emit();
      return;
    }
    if (this.frame !== undefined) return;
    this.frame = window.requestAnimationFrame(() => {
      this.frame = undefined;
      this.emit();
    });
  }

  private emit(): void {
    for (const listener of this.listeners) listener();
  }
}
