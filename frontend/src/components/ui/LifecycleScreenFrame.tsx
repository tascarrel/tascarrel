import type { ReactNode } from "react";

export function LifecycleScreenFrame({
  children,
  danger = false,
  icon,
  log,
  title,
}: {
  children?: ReactNode;
  danger?: boolean;
  icon: ReactNode;
  log?: ReactNode;
  title: string;
}) {
  return (
    <div className="flex min-h-full items-center justify-center px-6 py-10 sm:px-10 sm:py-14">
      <div className="w-full max-w-3xl">
        <section className="text-center">
          <div className="inline-flex items-center gap-2.5">
            <span className={danger ? "text-red-300" : "text-accent-text"}>
              {icon}
            </span>
            <h1 className="text-base font-medium tracking-[-0.01em] text-foreground">{title}</h1>
          </div>
          {children ? <div className="mt-4">{children}</div> : null}
        </section>
        {log ? <div className="mt-7">{log}</div> : null}
      </div>
    </div>
  );
}
