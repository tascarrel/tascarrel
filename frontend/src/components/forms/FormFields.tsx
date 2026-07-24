import { useId } from "react";

import type { pods } from "../../api/generated/index.ts";
import { PodChooser } from "../../features/pods/PodChooser.tsx";
import { TextInput } from "../ui/TextInput.tsx";
import { useFieldContext } from "./FormContext.ts";

interface FieldPresentationProps {
  label: string;
  description?: string;
  disabled?: boolean;
  required?: boolean;
}

export function TextField({
  label,
  description,
  disabled = false,
  required = false,
  placeholder,
  maxLength,
}: FieldPresentationProps & {
  placeholder?: string;
  maxLength?: number;
}) {
  const field = useFieldContext<string>();
  const controlId = useId();
  const errors = visibleErrors(field.state.meta.isTouched, field.state.meta.errors);
  const describedBy = fieldDescriptionIds(controlId, description, errors);

  return (
    <div className="grid min-w-0 gap-1">
      <FieldLabel controlId={controlId} label={label} required={required} />
      <TextInput
        className="w-full bg-surface-raised px-2.5"
        id={controlId}
        name={field.name}
        value={field.state.value}
        required={required}
        disabled={disabled}
        placeholder={placeholder}
        maxLength={maxLength}
        aria-describedby={describedBy}
        aria-invalid={errors.length > 0 || undefined}
        onBlur={field.handleBlur}
        onChange={(event) => field.handleChange(event.target.value)}
      />
      <FieldMessages controlId={controlId} description={description} errors={errors} />
    </div>
  );
}

export function PortField({
  label,
  description,
  disabled = false,
  required = true,
  placeholder = "3000",
}: FieldPresentationProps & {
  placeholder?: string;
}) {
  const field = useFieldContext<string>();
  const controlId = useId();
  const errors = visibleErrors(field.state.meta.isTouched, field.state.meta.errors);
  const describedBy = fieldDescriptionIds(controlId, description, errors);

  return (
    <div className="grid min-w-0 gap-1">
      <FieldLabel controlId={controlId} label={label} required={required} />
      <TextInput
        className="w-full bg-surface-raised px-2.5 font-mono"
        id={controlId}
        name={field.name}
        type="number"
        inputMode="numeric"
        min={1}
        max={65535}
        value={field.state.value}
        required={required}
        disabled={disabled}
        placeholder={placeholder}
        aria-describedby={describedBy}
        aria-invalid={errors.length > 0 || undefined}
        onBlur={field.handleBlur}
        onChange={(event) => field.handleChange(event.target.value)}
      />
      <FieldMessages controlId={controlId} description={description} errors={errors} />
    </div>
  );
}

export function PodChooserField({
  label,
  pods: workspacePods,
  description,
  disabled = false,
  required = true,
}: FieldPresentationProps & {
  pods: readonly pods.Pod[];
}) {
  const field = useFieldContext<string>();
  const controlId = useId();
  const errors = visibleErrors(field.state.meta.isTouched, field.state.meta.errors);
  const describedBy = fieldDescriptionIds(controlId, description, errors);

  return (
    <div className="grid min-w-0 gap-1">
      <PodChooser
        id={controlId}
        name={field.name}
        label={fieldLabel(label, required)}
        pods={workspacePods}
        value={field.state.value}
        required={required}
        disabled={disabled}
        invalid={errors.length > 0}
        ariaDescribedBy={describedBy}
        onBlur={field.handleBlur}
        onChange={field.handleChange}
      />
      <FieldMessages controlId={controlId} description={description} errors={errors} />
    </div>
  );
}

function FieldLabel({
  controlId,
  label,
  required,
}: {
  controlId: string;
  label: string;
  required: boolean;
}) {
  return (
    <label className="text-[10px] text-subtle" htmlFor={controlId}>
      {fieldLabel(label, required)}
    </label>
  );
}

function FieldMessages({
  controlId,
  description,
  errors,
}: {
  controlId: string;
  description?: string;
  errors: readonly string[];
}) {
  return (
    <>
      {description ? (
        <p className="text-[10px] leading-4 text-subtle" id={`${controlId}-description`}>
          {description}
        </p>
      ) : null}
      {errors.length > 0 ? (
        <ul
          className="m-0 list-none p-0 text-[10px] leading-4 text-red-300"
          id={`${controlId}-error`}
          role="alert"
        >
          {errors.map((error, index) => <li key={`${index}:${error}`}>{error}</li>)}
        </ul>
      ) : null}
    </>
  );
}

function fieldLabel(label: string, required: boolean): string {
  return required ? label : `${label} (optional)`;
}

function fieldDescriptionIds(
  controlId: string,
  description: string | undefined,
  errors: readonly string[],
): string | undefined {
  const ids = [
    description ? `${controlId}-description` : undefined,
    errors.length > 0 ? `${controlId}-error` : undefined,
  ].filter((id): id is string => id !== undefined);
  return ids.length > 0 ? ids.join(" ") : undefined;
}

function visibleErrors(isTouched: boolean, errors: readonly unknown[]): string[] {
  if (!isTouched) return [];
  return errors
    .filter((error): error is NonNullable<typeof error> => error !== undefined && error !== null)
    .map((error) => typeof error === "string" ? error : String(error));
}
