import { AlertDialog } from "@base-ui/react/alert-dialog";
import { LoaderCircle } from "lucide-react";
import type { ReactNode } from "react";

import { Button } from "./Button.tsx";

export function ConfirmDialog({
  open,
  title,
  description,
  confirmLabel,
  pending = false,
  destructive = false,
  layer = "default",
  onOpenChange,
  onConfirm,
}: {
  open: boolean;
  title: ReactNode;
  description: ReactNode;
  confirmLabel: string;
  pending?: boolean;
  destructive?: boolean;
  layer?: "default" | "overlay";
  onOpenChange: (open: boolean) => void;
  onConfirm: () => void;
}) {
  return (
    <AlertDialog.Root
      open={open}
      onOpenChange={(nextOpen) => {
        if (!pending) onOpenChange(nextOpen);
      }}
    >
      <AlertDialog.Portal>
        <AlertDialog.Backdrop className={`fixed inset-0 bg-black/75 backdrop-blur-sm transition-opacity data-[ending-style]:opacity-0 data-[starting-style]:opacity-0 ${layer === "overlay" ? "z-[80]" : "z-50"}`} />
        <AlertDialog.Viewport className={`fixed inset-0 grid place-items-center overflow-y-auto p-4 ${layer === "overlay" ? "z-[80]" : "z-50"}`}>
          <AlertDialog.Popup
            className={`w-full max-w-md rounded-2xl border bg-surface-raised p-5 text-foreground shadow-2xl shadow-black/70 outline-none transition-[transform,opacity] data-[ending-style]:scale-95 data-[ending-style]:opacity-0 data-[starting-style]:scale-95 data-[starting-style]:opacity-0 ${
              destructive ? "border-red-500/25" : "border-ui-border-strong"
            }`}
          >
            <AlertDialog.Title className="text-base font-semibold">{title}</AlertDialog.Title>
            <AlertDialog.Description className="mt-2 text-sm leading-6 text-muted">
              {description}
            </AlertDialog.Description>
            <div className="mt-5 flex justify-end gap-2">
              <AlertDialog.Close
                className="rounded-xl border border-ui-border bg-surface px-3.5 py-2 text-sm font-medium text-muted transition hover:border-ui-border-strong hover:text-foreground disabled:cursor-not-allowed disabled:opacity-50"
                disabled={pending}
              >
                Cancel
              </AlertDialog.Close>
              <Button
                className="min-w-20 rounded-xl px-3.5 py-2 text-sm"
                variant={destructive ? "danger" : "primary"}
                disabled={pending}
                onClick={onConfirm}
              >
                {pending ? <LoaderCircle aria-hidden="true" className="size-3.5 animate-spin" /> : null}
                {confirmLabel}
              </Button>
            </div>
          </AlertDialog.Popup>
        </AlertDialog.Viewport>
      </AlertDialog.Portal>
    </AlertDialog.Root>
  );
}
