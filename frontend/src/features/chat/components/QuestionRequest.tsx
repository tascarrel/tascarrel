import { SendHorizontal } from "lucide-react";
import { type FormEvent, useState } from "react";

import type { chats } from "../../../api/generated/index.ts";
import { Button } from "../../../components/ui/Button.tsx";

export function QuestionRequest({
  request,
  enabled,
  resolving,
  onResolve,
}: {
  request: chats.ChatRequest;
  enabled: boolean;
  resolving: boolean;
  onResolve: (
    requestId: chats.ChatRequestId,
    answers: chats.ChatQuestionAnswer[],
  ) => Promise<void>;
}) {
  const [answers, setAnswers] = useState<Record<string, string[]>>({});
  const complete = request.questions.every(
    (question) => (answers[String(question.questionId)] ?? []).some((answer) => answer.trim()),
  );

  const submit = (event: FormEvent) => {
    event.preventDefault();
    if (!enabled || !complete) return;
    void onResolve(
      request.requestId,
      request.questions.map((question) => ({
        questionId: question.questionId,
        answers: (answers[String(question.questionId)] ?? []).filter(Boolean),
      })),
    );
  };

  return (
    <form
      className={`mx-auto my-5 max-w-3xl overflow-hidden rounded-2xl border ${
        request.resolved ? "border-ui-border bg-surface" : "border-accent/30 bg-accent/5"
      }`}
      onSubmit={submit}
    >
      <div className="border-b border-ui-border px-4 py-3">
        <div>
          <div className="text-sm font-semibold text-foreground">Agent input requested</div>
          <div className="text-xs text-subtle">
            {request.resolved
              ? "Answered"
              : enabled
                ? "The active agent is waiting for your response"
                : "This request belongs to an inactive binding"}
          </div>
        </div>
      </div>

      <div className="space-y-5 p-4">
        {request.questions.map((question) => (
          <QuestionField
            key={question.questionId}
            question={question}
            values={answers[String(question.questionId)] ?? []}
            disabled={!enabled || resolving}
            onChange={(values) =>
              setAnswers((current) => ({ ...current, [String(question.questionId)]: values }))
            }
          />
        ))}
      </div>

      {!request.resolved ? (
        <div className="flex justify-end border-t border-ui-border px-4 py-3">
          <Button
            className="rounded-xl px-3.5 py-2"
            type="submit"
            variant="primary"
            disabled={!enabled || !complete || resolving}
          >
            <SendHorizontal aria-hidden="true" className="size-3.5" />
            {resolving ? "Sending…" : "Send answer"}
          </Button>
        </div>
      ) : null}
    </form>
  );
}

function QuestionField({
  question,
  values,
  disabled,
  onChange,
}: {
  question: chats.ChatQuestion;
  values: string[];
  disabled: boolean;
  onChange: (values: string[]) => void;
}) {
  const type = question.multiple ? "checkbox" : "radio";
  return (
    <fieldset disabled={disabled}>
      <legend className="text-sm font-semibold text-foreground">{question.header}</legend>
      <p className="mb-3 mt-1 text-xs leading-5 text-muted">{question.prompt}</p>
      {question.options.length ? (
        <div className="grid gap-2 sm:grid-cols-2">
          {question.options.map((option) => {
            const checked = values.includes(option.label);
            return (
              <label
                className={`flex cursor-pointer gap-3 rounded-xl border px-3 py-2.5 transition ${
                  checked
                    ? "border-accent/45 bg-accent/10"
                    : "border-ui-border bg-surface-raised hover:border-ui-border-strong"
                } disabled:cursor-not-allowed disabled:opacity-50`}
                key={option.label}
              >
                <input
                  className="mt-0.5 size-3.5 accent-brand"
                  type={type}
                  name={String(question.questionId)}
                  checked={checked}
                  onChange={(event) => {
                    if (!question.multiple) {
                      onChange([option.label]);
                    } else if (event.target.checked) {
                      onChange([...values, option.label]);
                    } else {
                      onChange(values.filter((value) => value !== option.label));
                    }
                  }}
                />
                <span>
                  <span className="block text-xs font-medium text-foreground">{option.label}</span>
                  {option.description ? (
                    <span className="mt-0.5 block text-[11px] leading-4 text-subtle">
                      {option.description}
                    </span>
                  ) : null}
                </span>
              </label>
            );
          })}
        </div>
      ) : (
        <textarea
          className="min-h-20 w-full resize-y rounded-xl border border-ui-border bg-surface-raised px-3 py-2 text-sm text-foreground outline-none placeholder:text-subtle focus:border-accent/50"
          placeholder="Type your answer…"
          value={values[0] ?? ""}
          onChange={(event) => onChange([event.target.value])}
        />
      )}
    </fieldset>
  );
}
