import { Plus } from "lucide-react";
import { useId, useState } from "react";

import { hostApi } from "../../api/client.ts";
import type { network, pods, workspaces } from "../../api/generated/index.ts";
import { focusFirstInvalidField, useAppForm } from "../../components/forms/index.ts";

export function HttpRouteForm({ workspace, pods: workspacePods }: NetworkFormProps) {
  const formId = useId();
  const [submissionError, setSubmissionError] = useState<string>();
  const form = useAppForm({
    defaultValues: {
      podId: firstPodId(workspacePods),
      podPort: "",
      title: "",
    },
    onSubmitInvalid: () => focusFirstInvalidField(formId),
    onSubmit: async ({ value, formApi }) => {
      setSubmissionError(undefined);
      const podPort = parsePort(value.podPort);
      if (podPort === undefined) return;
      try {
        await hostApi.execute("network_CreateHttpRoute", {
          workspace,
          podId: value.podId as pods.PodId,
          podPort,
          title: value.title.trim(),
          internal: false,
        });
        formApi.reset({ podId: value.podId, podPort: "", title: "" });
      } catch (cause) {
        setSubmissionError(errorMessage(cause));
      }
    },
  });

  return (
    <form.AppForm>
      <form.FormRoot className="max-w-xl" id={formId}>
        <form.FormFields>
          <form.AppField
            name="podId"
            validators={{
              onChange: ({ value }) => value ? undefined : "Select a target pod.",
            }}
          >
            {(field) => (
              <field.PodChooserField
                label="Target pod"
                pods={workspacePods}
                disabled={workspacePods.length === 0}
              />
            )}
          </form.AppField>
          <form.AppField
            name="podPort"
            validators={{
              onChange: ({ value }) => portError(value, "Pod port"),
            }}
          >
            {(field) => <field.PortField label="Pod port" disabled={workspacePods.length === 0} />}
          </form.AppField>
          <form.AppField
            name="title"
            validators={{
              onChange: ({ value }) => titleError(value, true),
            }}
          >
            {(field) => (
              <field.TextField
                label="Title"
                required
                maxLength={NETWORK_TITLE_MAX_BYTES}
                placeholder="Development server"
                disabled={workspacePods.length === 0}
              />
            )}
          </form.AppField>
        </form.FormFields>
        <form.SubmissionError>{submissionError}</form.SubmissionError>
        <form.SubmitButton
          className="mt-4"
          label="Add route"
          submittingLabel="Creating…"
          icon={<Plus aria-hidden="true" className="size-3.5" />}
          disabled={workspacePods.length === 0}
        />
      </form.FormRoot>
    </form.AppForm>
  );
}

export function HostPodForwardForm({ workspace, pods: workspacePods }: NetworkFormProps) {
  const formId = useId();
  const [submissionError, setSubmissionError] = useState<string>();
  const form = useAppForm({
    defaultValues: {
      podId: firstPodId(workspacePods),
      podPort: "",
      title: "",
    },
    onSubmitInvalid: () => focusFirstInvalidField(formId),
    onSubmit: async ({ value, formApi }) => {
      setSubmissionError(undefined);
      const podPort = parsePort(value.podPort);
      if (podPort === undefined) return;
      try {
        await hostApi.execute("network_CreatePortForward", {
          workspace,
          podId: value.podId as pods.PodId,
          podPort,
          ...(value.title.trim() ? { title: value.title.trim() } : {}),
        });
        formApi.reset({ podId: value.podId, podPort: "", title: "" });
      } catch (cause) {
        setSubmissionError(errorMessage(cause));
      }
    },
  });

  return (
    <form.AppForm>
      <form.FormRoot className="max-w-xl" id={formId}>
        <form.FormFields>
          <form.AppField
            name="podId"
            validators={{
              onChange: ({ value }) => value ? undefined : "Select a target pod.",
            }}
          >
            {(field) => (
              <field.PodChooserField
                label="Target pod"
                pods={workspacePods}
                disabled={workspacePods.length === 0}
              />
            )}
          </form.AppField>
          <form.AppField
            name="podPort"
            validators={{
              onChange: ({ value }) => portError(value, "Pod port"),
            }}
          >
            {(field) => <field.PortField label="Pod port" disabled={workspacePods.length === 0} />}
          </form.AppField>
          <form.AppField
            name="title"
            validators={{
              onChange: ({ value }) => titleError(value, false),
            }}
          >
            {(field) => (
              <field.TextField
                label="Title"
                maxLength={NETWORK_TITLE_MAX_BYTES}
                placeholder="Development server"
                disabled={workspacePods.length === 0}
              />
            )}
          </form.AppField>
        </form.FormFields>
        <form.SubmissionError>{submissionError}</form.SubmissionError>
        <form.SubmitButton
          className="mt-4"
          label="Add forward"
          submittingLabel="Creating…"
          icon={<Plus aria-hidden="true" className="size-3.5" />}
          disabled={workspacePods.length === 0}
        />
      </form.FormRoot>
    </form.AppForm>
  );
}

export function PodHostForwardForm({ workspace, pods: workspacePods }: NetworkFormProps) {
  const formId = useId();
  const [submissionError, setSubmissionError] = useState<string>();
  const form = useAppForm({
    defaultValues: {
      podId: firstPodId(workspacePods),
      podVisiblePort: "",
      hostPort: "",
      title: "",
    },
    onSubmitInvalid: () => focusFirstInvalidField(formId),
    onSubmit: async ({ value, formApi }) => {
      setSubmissionError(undefined);
      const podVisiblePort = parsePort(value.podVisiblePort);
      const hostPort = parsePort(value.hostPort);
      if (podVisiblePort === undefined || hostPort === undefined) return;
      try {
        await hostApi.execute("network_CreatePodHostForward", {
          workspace,
          podId: value.podId as pods.PodId,
          mapping: `${hostPort}:${podVisiblePort}` as network.PortMapping,
          ...(value.title.trim() ? { title: value.title.trim() } : {}),
        });
        formApi.reset({
          podId: value.podId,
          podVisiblePort: "",
          hostPort: "",
          title: "",
        });
      } catch (cause) {
        setSubmissionError(errorMessage(cause));
      }
    },
  });

  return (
    <form.AppForm>
      <form.FormRoot className="max-w-xl" id={formId}>
        <form.FormFields>
          <form.AppField
            name="podId"
            validators={{
              onChange: ({ value }) => value ? undefined : "Select a source pod.",
            }}
          >
            {(field) => (
              <field.PodChooserField
                label="Source pod"
                pods={workspacePods}
                disabled={workspacePods.length === 0}
              />
            )}
          </form.AppField>
          <form.AppField
            name="podVisiblePort"
            validators={{
              onChange: ({ value }) => portError(value, "Pod-visible port"),
            }}
          >
            {(field) => (
              <field.PortField
                label="Pod-visible port"
                placeholder="8272"
                disabled={workspacePods.length === 0}
              />
            )}
          </form.AppField>
          <form.AppField
            name="hostPort"
            validators={{
              onChange: ({ value }) => portError(value, "Host port"),
            }}
          >
            {(field) => (
              <field.PortField
                label="Host port"
                placeholder="8272"
                disabled={workspacePods.length === 0}
              />
            )}
          </form.AppField>
          <form.AppField
            name="title"
            validators={{
              onChange: ({ value }) => titleError(value, false),
            }}
          >
            {(field) => (
              <field.TextField
                label="Title"
                maxLength={NETWORK_TITLE_MAX_BYTES}
                placeholder="Tascarrel backend"
                disabled={workspacePods.length === 0}
              />
            )}
          </form.AppField>
        </form.FormFields>
        <form.SubmissionError>{submissionError}</form.SubmissionError>
        <form.SubmitButton
          className="mt-4"
          label="Add forward"
          submittingLabel="Creating…"
          icon={<Plus aria-hidden="true" className="size-3.5" />}
          disabled={workspacePods.length === 0}
        />
      </form.FormRoot>
    </form.AppForm>
  );
}

interface NetworkFormProps {
  workspace: workspaces.WorkspaceName;
  pods: readonly pods.Pod[];
}

const NETWORK_TITLE_MAX_BYTES = 256;

function firstPodId(workspacePods: readonly pods.Pod[]): string {
  return String(workspacePods[0]?.id ?? "");
}

function parsePort(value: string): network.CreateHttpRouteAction["podPort"] | undefined {
  const normalized = value.trim();
  if (!/^\d+$/.test(normalized)) return undefined;
  const port = Number(normalized);
  return Number.isInteger(port) && port >= 1 && port <= 65535
    ? port as network.CreateHttpRouteAction["podPort"]
    : undefined;
}

function portError(value: string, label: string): string | undefined {
  return parsePort(value) === undefined ? `${label} must be between 1 and 65535.` : undefined;
}

function titleError(value: string, required: boolean): string | undefined {
  const title = value.trim();
  if (required && !title) return "Enter a route title.";
  return new TextEncoder().encode(title).length > NETWORK_TITLE_MAX_BYTES
    ? `Title must not exceed ${NETWORK_TITLE_MAX_BYTES} bytes.`
    : undefined;
}

function errorMessage(cause: unknown): string {
  return cause instanceof Error ? cause.message : String(cause);
}
