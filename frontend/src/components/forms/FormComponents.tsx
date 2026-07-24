import { LoaderCircle } from "lucide-react";
import type { FormHTMLAttributes, ReactNode } from "react";

import { Button } from "../ui/Button.tsx";
import { useFormContext } from "./FormContext.ts";

export function FormRoot({
  children,
  className = "",
  ...props
}: Omit<FormHTMLAttributes<HTMLFormElement>, "noValidate" | "onSubmit">) {
  const form = useFormContext();
  return (
    <form
      className={className}
      noValidate
      onSubmit={(event) => {
        event.preventDefault();
        event.stopPropagation();
        void form.handleSubmit();
      }}
      {...props}
    >
      {children}
    </form>
  );
}

export function SubmitButton({
  label,
  submittingLabel,
  icon,
  disabled = false,
  className = "",
}: {
  label: string;
  submittingLabel: string;
  icon?: ReactNode;
  disabled?: boolean;
  className?: string;
}) {
  const form = useFormContext();
  return (
    <form.Subscribe selector={(state) => state.isSubmitting}>
      {(isSubmitting) => (
        <Button
          className={className}
          type="submit"
          size="small"
          variant="primary"
          disabled={disabled || isSubmitting}
        >
          {isSubmitting
            ? <LoaderCircle aria-hidden="true" className="size-3.5 animate-spin" />
            : icon}
          {isSubmitting ? submittingLabel : label}
        </Button>
      )}
    </form.Subscribe>
  );
}

export function FormFields({ children }: { children: ReactNode }) {
  return <div className="grid gap-3">{children}</div>;
}

export function SubmissionError({ children }: { children?: ReactNode }) {
  return children ? (
    <p className="mt-3 text-xs leading-5 text-red-300" role="alert">{children}</p>
  ) : null;
}

export function focusFirstInvalidField(formId: string): void {
  window.requestAnimationFrame(() => {
    document
      .getElementById(formId)
      ?.querySelector<HTMLElement>("[aria-invalid='true']")
      ?.focus();
  });
}
