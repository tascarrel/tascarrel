import {
  createContext,
  type ComponentProps,
  type Context,
  type CSSProperties,
  type FocusEventHandler,
  type ReactNode,
  useCallback,
  useContext,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import { createPortal } from "react-dom";

type ManagedIframeProps = Omit<
  ComponentProps<"iframe">,
  | "aria-hidden"
  | "children"
  | "className"
  | "key"
  | "onBlur"
  | "onFocus"
  | "ref"
  | "src"
  | "style"
  | "tabIndex"
  | "title"
>;

export type IframeFrameSpec = {
  id: string;
  src: string;
  title: string;
  revision?: number | string;
  background?: "application" | "document";
  iframeProps?: ManagedIframeProps;
};

type RetainedFrame = IframeFrameSpec & {
  lastActivated: number;
};

type ActiveFrame = {
  id: string;
  anchor: HTMLElement;
  activation: number;
};

type IframePoolContextValue = {
  activate: (spec: IframeFrameSpec, anchor: HTMLElement) => () => void;
  retainFrames: (ids: readonly string[]) => void;
};

export type IframePool = Context<IframePoolContextValue | undefined>;

/** Creates an identity for one independently configured iframe pool. */
export function createIframePool(): IframePool {
  return createContext<IframePoolContextValue | undefined>(undefined);
}

/** Retains iframe elements outside their feature layout and positions the active one over an anchor. */
export function IframePoolProvider({
  children,
  maxFrames,
  pool,
}: {
  children: ReactNode;
  maxFrames?: number;
  pool: IframePool;
}) {
  if (maxFrames !== undefined && (!Number.isInteger(maxFrames) || maxFrames < 1)) {
    throw new Error("Iframe pool maximum must be a positive integer");
  }

  const [frames, setFrames] = useState<readonly RetainedFrame[]>([]);
  const [active, setActive] = useState<ActiveFrame>();
  const clock = useRef(0);
  const activationClock = useRef(0);
  const retainFrames = useCallback((ids: readonly string[]) => {
    const retainedFrameIds = new Set(ids);
    setFrames((current) => {
      const retained = current.filter((frame) => retainedFrameIds.has(frame.id));
      return retained.length === current.length ? current : retained;
    });
    setActive((current) => current && retainedFrameIds.has(current.id) ? current : undefined);
  }, []);

  const activate = useCallback((spec: IframeFrameSpec, anchor: HTMLElement) => {
    const lastActivated = ++clock.current;
    const activation = ++activationClock.current;
    setFrames((current) => retainFrame(current, { ...spec, lastActivated }, maxFrames));
    setActive({ id: spec.id, anchor, activation });
    return () => {
      setActive((current) => current?.activation === activation ? undefined : current);
    };
  }, [maxFrames]);
  const value = useMemo(() => ({ activate, retainFrames }), [activate, retainFrames]);
  const PoolContext = pool;

  return (
    <PoolContext.Provider value={value}>
      {children}
      <IframeDeck active={active} frames={frames} />
    </PoolContext.Provider>
  );
}

/** Activates a retained iframe over `anchor` until the caller changes it or unmounts. */
export function useIframeFrame(
  pool: IframePool,
  spec: IframeFrameSpec | undefined,
  anchor: HTMLElement | null,
) {
  const context = useContext(pool);
  if (!context) throw new Error("Iframe pool provider is missing");

  useLayoutEffect(() => {
    if (!spec || !anchor) return;
    return context.activate(spec, anchor);
  }, [anchor, context, spec]);
}

/** Removes retained frames that are no longer present in the owning feature. */
export function useRetainedIframeFrames(
  pool: IframePool,
  retainedFrameIds: readonly string[],
) {
  const context = useContext(pool);
  if (!context) throw new Error("Iframe pool provider is missing");

  useLayoutEffect(() => {
    context.retainFrames(retainedFrameIds);
  }, [context, retainedFrameIds]);
}

function IframeDeck({
  active,
  frames,
}: {
  active?: ActiveFrame;
  frames: readonly RetainedFrame[];
}) {
  const bounds = useAnchorBounds(active?.anchor);
  return createPortal(
    frames.map((frame) => (
      <IframeLayer
        active={active?.id === frame.id && bounds !== undefined}
        bounds={bounds}
        frame={frame}
        key={`${frame.id}:${frame.revision ?? ""}`}
      />
    )),
    document.body,
  );
}

function IframeLayer({
  active,
  bounds,
  frame,
}: {
  active: boolean;
  bounds?: AnchorBounds;
  frame: RetainedFrame;
}) {
  const iframeFocus = useIframeFocus();
  const focused = active && iframeFocus.focused;
  const style: CSSProperties | undefined = active && bounds ? {
    left: bounds.left,
    top: bounds.top,
    width: bounds.width,
    height: bounds.height,
  } : undefined;

  return (
    <>
      <IframeFocusBackdrop visible={focused} />
      <div
        className="iframe-pool-layer"
        data-active={active || undefined}
        data-background={frame.background}
        data-iframe-focused={focused || undefined}
        style={style}
      >
        <iframe
          {...frame.iframeProps}
          aria-hidden={!active}
          ref={iframeFocus.frameRef}
          src={frame.src}
          tabIndex={active ? 0 : -1}
          title={frame.title}
          onBlur={iframeFocus.onBlur}
          onFocus={iframeFocus.onFocus}
        />
      </div>
    </>
  );
}

type AnchorBounds = Pick<DOMRect, "height" | "left" | "top" | "width">;

function useAnchorBounds(anchor: HTMLElement | undefined): AnchorBounds | undefined {
  const [bounds, setBounds] = useState<AnchorBounds>();

  useLayoutEffect(() => {
    if (!anchor) {
      setBounds(undefined);
      return;
    }
    const update = () => {
      const next = anchor.getBoundingClientRect();
      setBounds(next.width > 0 && next.height > 0 ? {
        left: next.left,
        top: next.top,
        width: next.width,
        height: next.height,
      } : undefined);
    };
    update();
    const observer = new ResizeObserver(update);
    observer.observe(anchor);
    window.addEventListener("resize", update);
    window.addEventListener("scroll", update, true);
    return () => {
      observer.disconnect();
      window.removeEventListener("resize", update);
      window.removeEventListener("scroll", update, true);
    };
  }, [anchor]);

  return bounds;
}

function useIframeFocus() {
  const frameRef = useRef<HTMLIFrameElement>(null);
  const animationFrame = useRef<number>(undefined);
  const [focused, setFocused] = useState(false);
  const syncFocus = useCallback(() => {
    setFocused(document.activeElement === frameRef.current);
  }, []);
  const scheduleFocusSync = useCallback(() => {
    if (animationFrame.current !== undefined) cancelAnimationFrame(animationFrame.current);
    animationFrame.current = requestAnimationFrame(() => {
      animationFrame.current = undefined;
      syncFocus();
    });
  }, [syncFocus]);

  useEffect(() => {
    window.addEventListener("blur", scheduleFocusSync);
    window.addEventListener("focus", scheduleFocusSync);
    document.addEventListener("focusin", scheduleFocusSync);
    document.addEventListener("focusout", scheduleFocusSync);
    return () => {
      window.removeEventListener("blur", scheduleFocusSync);
      window.removeEventListener("focus", scheduleFocusSync);
      document.removeEventListener("focusin", scheduleFocusSync);
      document.removeEventListener("focusout", scheduleFocusSync);
      if (animationFrame.current !== undefined) cancelAnimationFrame(animationFrame.current);
    };
  }, [scheduleFocusSync]);

  const onFocus: FocusEventHandler<HTMLIFrameElement> = () => setFocused(true);
  const onBlur: FocusEventHandler<HTMLIFrameElement> = () => scheduleFocusSync();
  return { focused, frameRef, onBlur, onFocus };
}

function IframeFocusBackdrop({ visible }: { visible: boolean }) {
  if (!visible) return null;
  return createPortal(
    <div className="iframe-focus-backdrop" aria-hidden="true" />,
    document.body,
  );
}

function retainFrame(
  current: readonly RetainedFrame[],
  next: RetainedFrame,
  maxFrames: number | undefined,
): readonly RetainedFrame[] {
  const retained = [
    ...current.filter((frame) => frame.id !== next.id),
    next,
  ];
  if (maxFrames === undefined || retained.length <= maxFrames) return retained;
  const oldest = retained
    .filter((frame) => frame.id !== next.id)
    .toSorted((left, right) => left.lastActivated - right.lastActivated)[0];
  return oldest ? retained.filter((frame) => frame.id !== oldest.id) : retained;
}
