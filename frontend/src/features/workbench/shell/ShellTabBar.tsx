import { ChevronLeft, ChevronRight, Plus, X, type LucideIcon } from "lucide-react";
import {
  type ButtonHTMLAttributes,
  type HTMLAttributes,
  type ReactNode,
  useCallback,
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
} from "react";

type ShellTabStripProps = HTMLAttributes<HTMLDivElement> & {
  label: string;
  action?: ReactNode;
};

export function ShellTabStrip({
  label,
  action,
  className,
  children,
  onScroll,
  onWheel,
  ...props
}: ShellTabStripProps) {
  const scrollerRef = useRef<HTMLDivElement>(null);
  const stripRef = useRef<HTMLDivElement>(null);
  const [scroll, setScroll] = useState({ overflow: false, backward: false, forward: false });

  const updateScroll = useCallback(() => {
    const scroller = scrollerRef.current;
    const strip = stripRef.current;
    if (!scroller || !strip) return;
    const maximum = Math.max(0, strip.scrollWidth - strip.clientWidth);
    const scrollControlWidth = Array.from(
      scroller.querySelectorAll<HTMLElement>(".shell-tab-scroll-button"),
    ).reduce((width, control) => width + control.getBoundingClientRect().width, 0);
    const next = {
      overflow: strip.scrollWidth > strip.clientWidth + scrollControlWidth + 1,
      backward: strip.scrollLeft > 1,
      forward: strip.scrollLeft < maximum - 1,
    };
    setScroll((current) =>
      current.overflow === next.overflow
        && current.backward === next.backward
        && current.forward === next.forward
        ? current
        : next
    );
  }, []);

  const revealSelectedTab = useCallback((behavior: ScrollBehavior) => {
    const strip = stripRef.current;
    const selected = strip?.querySelector<HTMLElement>('[data-selected="true"]');
    if (!strip || !selected) return;
    const stripBounds = strip.getBoundingClientRect();
    const selectedBounds = selected.getBoundingClientRect();
    if (selectedBounds.left < stripBounds.left) {
      strip.scrollBy({ left: selectedBounds.left - stripBounds.left, behavior });
    } else if (selectedBounds.right > stripBounds.right) {
      strip.scrollBy({ left: selectedBounds.right - stripBounds.right, behavior });
    }
  }, []);

  useLayoutEffect(() => {
    revealSelectedTab("smooth");
    updateScroll();
  }, [children, revealSelectedTab, updateScroll]);

  useEffect(() => {
    const scroller = scrollerRef.current;
    const strip = stripRef.current;
    const list = strip?.querySelector<HTMLElement>(".shell-tab-list");
    if (!scroller || !strip || !list) return;
    const observer = new ResizeObserver(() => {
      revealSelectedTab("auto");
      updateScroll();
    });
    observer.observe(scroller);
    observer.observe(strip);
    observer.observe(list);
    return () => observer.disconnect();
  }, [revealSelectedTab, updateScroll]);

  const pageTabs = (direction: -1 | 1) => {
    const strip = stripRef.current;
    if (!strip) return;
    strip.scrollBy({
      left: direction * Math.max(180, strip.clientWidth * 0.7),
      behavior: "smooth",
    });
  };

  return (
    <div className="shell-tab-scroller" ref={scrollerRef}>
      {scroll.overflow ? (
        <ShellTabScrollButton
          direction={-1}
          label={`Scroll ${label} left`}
          disabled={!scroll.backward}
          onClick={() => pageTabs(-1)}
        />
      ) : null}
      <div
        {...props}
        className={`shell-tab-strip ${className ?? ""}`}
        ref={stripRef}
        onScroll={(event) => {
          onScroll?.(event);
          updateScroll();
        }}
        onWheel={(event) => {
          onWheel?.(event);
          if (event.defaultPrevented) return;
          const strip = event.currentTarget;
          if (
            strip.scrollWidth <= strip.clientWidth
            || Math.abs(event.deltaX) >= Math.abs(event.deltaY)
          ) return;
          const maximum = strip.scrollWidth - strip.clientWidth;
          const next = Math.max(0, Math.min(maximum, strip.scrollLeft + event.deltaY));
          if (next === strip.scrollLeft) return;
          event.preventDefault();
          strip.scrollLeft = next;
        }}
      >
        <div className="shell-tab-list" role="group" aria-label={label}>
          {children}
        </div>
      </div>
      {scroll.overflow ? (
        <ShellTabScrollButton
          direction={1}
          label={`Scroll ${label} right`}
          disabled={!scroll.forward}
          onClick={() => pageTabs(1)}
        />
      ) : null}
      {action}
    </div>
  );
}

type ShellTabProps = ButtonHTMLAttributes<HTMLButtonElement> & {
  active?: boolean;
  attention?: boolean;
  failure?: boolean;
  closeLabel?: string;
  onClose?: () => void;
};

export function ShellTab({
  active = false,
  attention = false,
  failure = false,
  className,
  children,
  closeLabel,
  onClose,
  ...props
}: ShellTabProps) {
  const interactive = Boolean(props.onClick) && !props.disabled;
  return (
    <div
      className={`shell-tab ${active ? "shell-tab-active" : ""} ${attention ? "shell-tab-attention" : ""} ${failure ? "shell-tab-failure" : ""} ${onClose ? "shell-tab-has-close" : ""} ${className ?? ""}`}
      data-attention={attention || undefined}
      data-interactive={interactive || undefined}
      data-selected={active || undefined}
    >
      <button
        {...props}
        className="shell-tab-target"
        type="button"
        aria-pressed={active}
        disabled={props.disabled || !props.onClick}
      >
        {children}
      </button>
      {onClose && closeLabel ? (
        <button
          className="shell-tab-close"
          type="button"
          aria-label={closeLabel}
          title={closeLabel}
          onClick={onClose}
        >
          <X aria-hidden="true" size={10} />
        </button>
      ) : null}
    </div>
  );
}

export function ShellTabAction({ label, icon: Icon = Plus, onClick, disabled }: {
  label: string;
  icon?: LucideIcon;
  onClick?: () => void;
  disabled?: boolean;
}) {
  return (
    <button
      className="shell-tab-action"
      type="button"
      aria-label={label}
      title={label}
      disabled={disabled}
      onClick={onClick}
    >
      <Icon aria-hidden="true" size={14} />
    </button>
  );
}

function ShellTabScrollButton({ direction, disabled, onClick, label }: {
  direction: -1 | 1;
  disabled: boolean;
  onClick: () => void;
  label: string;
}) {
  return (
    <button
      className="shell-tab-scroll-button"
      type="button"
      aria-label={label}
      title={label}
      disabled={disabled}
      onClick={onClick}
    >
      {direction < 0
        ? <ChevronLeft aria-hidden="true" size={14} />
        : <ChevronRight aria-hidden="true" size={14} />}
    </button>
  );
}
