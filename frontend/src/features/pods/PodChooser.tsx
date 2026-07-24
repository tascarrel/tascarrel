import type { pods } from "../../api/generated/index.ts";
import { SelectControl } from "../../components/ui/SelectControl.tsx";

export function PodChooser({
  label,
  pods: workspacePods,
  value,
  disabled = false,
  id,
  name,
  required = false,
  invalid = false,
  ariaDescribedBy,
  onBlur,
  onChange,
}: {
  label: string;
  pods: readonly pods.Pod[];
  value: string;
  disabled?: boolean;
  id?: string;
  name?: string;
  required?: boolean;
  invalid?: boolean;
  ariaDescribedBy?: string;
  onBlur?: () => void;
  onChange: (podId: string) => void;
}) {
  return (
    <SelectControl
      className="text-left"
      label={label}
      variant="default"
      id={id}
      name={name}
      value={value}
      required={required}
      options={workspacePods.map((pod) => ({
        label: pod.title || "Untitled pod",
        value: String(pod.id),
      }))}
      disabled={disabled}
      invalid={invalid}
      ariaDescribedBy={ariaDescribedBy}
      onBlur={onBlur}
      onChange={onChange}
    />
  );
}
