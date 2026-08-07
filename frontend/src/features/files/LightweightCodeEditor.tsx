import { Compartment, EditorState, type Extension } from "@codemirror/state";
import { EditorView, keymap } from "@codemirror/view";
import { basicSetup } from "codemirror";
import { useEffect, useLayoutEffect, useRef, useState } from "react";

/** A compact CodeMirror editor that loads language support only when needed. */
export default function LightweightCodeEditor({
  value,
  path,
  line,
  onChange,
  onSave,
}: {
  value: string;
  path: string;
  line?: number;
  onChange: (value: string) => void;
  onSave: () => void;
}) {
  const host = useRef<HTMLDivElement>(null);
  const view = useRef<EditorView | undefined>(undefined);
  const onChangeRef = useRef(onChange);
  const onSaveRef = useRef(onSave);
  const [languageError, setLanguageError] = useState<string>();

  useLayoutEffect(() => {
    onChangeRef.current = onChange;
    onSaveRef.current = onSave;
  });

  useEffect(() => {
    if (!host.current) return;
    setLanguageError(undefined);
    const language = new Compartment();
    const editor = new EditorView({
      parent: host.current,
      state: EditorState.create({
        doc: value,
        extensions: [
          basicSetup,
          language.of([]),
          keymap.of([{
            key: "Mod-s",
            preventDefault: true,
            run: () => {
              onSaveRef.current();
              return true;
            },
          }]),
          EditorView.contentAttributes.of({
            "aria-label": `Edit ${path}`,
            spellcheck: "false",
          }),
          EditorView.updateListener.of((update) => {
            if (update.docChanged) onChangeRef.current(update.state.doc.toString());
          }),
          editorTheme,
        ],
      }),
    });
    view.current = editor;
    revealLine(editor, line);

    let disposed = false;
    void languageForPath(path).then(
      (support) => {
        if (!disposed) editor.dispatch({ effects: language.reconfigure(support) });
      },
      (cause) => {
        if (!disposed) setLanguageError(errorMessage(cause));
      },
    );
    return () => {
      disposed = true;
      view.current = undefined;
      editor.destroy();
    };
  }, [path]);

  useEffect(() => {
    const editor = view.current;
    if (!editor) return;
    const current = editor.state.doc.toString();
    if (current !== value) {
      editor.dispatch({ changes: { from: 0, to: current.length, insert: value } });
    }
  }, [value]);

  useEffect(() => {
    if (view.current) revealLine(view.current, line);
  }, [line]);

  return (
    <div className="relative h-full min-h-0 overflow-hidden">
      <div className="h-full" ref={host} />
      {languageError ? (
        <p className="absolute inset-x-3 bottom-3 rounded-lg border border-amber-500/20 bg-surface-raised px-3 py-2 text-xs text-amber-200 shadow-lg" role="alert">
          Syntax support could not load. Plain-text editing remains available: {languageError}
        </p>
      ) : null}
    </div>
  );
}

const editorTheme = EditorView.theme({
  "&": {
    height: "100%",
    color: "var(--color-foreground)",
    backgroundColor: "var(--syntax-background)",
    fontSize: "12px",
  },
  "&.cm-focused": { outline: "none" },
  ".cm-scroller": {
    overflow: "auto",
    fontFamily:
      'ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, "Liberation Mono", monospace',
    lineHeight: "1.5",
  },
  ".cm-content": { minHeight: "100%", padding: "10px 0", caretColor: "var(--color-accent-text)" },
  ".cm-line": { padding: "0 14px" },
  ".cm-gutters": {
    color: "var(--color-subtle)",
    backgroundColor: "var(--color-surface)",
    borderRight: "1px solid var(--color-ui-border)",
  },
  ".cm-activeLine, .cm-activeLineGutter": { backgroundColor: "var(--color-accent-soft)" },
  ".cm-cursor, .cm-dropCursor": { borderLeftColor: "var(--color-accent-text)" },
  ".cm-selectionBackground, &.cm-focused .cm-selectionBackground, ::selection": {
    backgroundColor: "var(--color-accent-soft)",
  },
});

function revealLine(editor: EditorView, line?: number) {
  if (!line) {
    editor.focus();
    return;
  }
  const boundedLine = Math.max(1, Math.min(line, editor.state.doc.lines));
  const position = editor.state.doc.line(boundedLine).from;
  editor.dispatch({
    selection: { anchor: position },
    effects: EditorView.scrollIntoView(position, { y: "center" }),
  });
  editor.focus();
}

async function languageForPath(path: string): Promise<Extension> {
  const extension = path.toLowerCase().split(".").pop();
  switch (extension) {
    case "md":
    case "markdown":
    case "mdown":
    case "mkd":
      return (await import("@codemirror/lang-markdown")).markdown();
    case "js":
    case "jsx":
    case "mjs":
    case "cjs":
      return (await import("@codemirror/lang-javascript")).javascript({
        jsx: extension === "jsx",
      });
    case "ts":
    case "tsx":
    case "mts":
    case "cts":
      return (await import("@codemirror/lang-javascript")).javascript({
        jsx: extension === "tsx",
        typescript: true,
      });
    case "json":
    case "jsonc":
      return (await import("@codemirror/lang-json")).json();
    case "css":
    case "scss":
    case "less":
      return (await import("@codemirror/lang-css")).css();
    case "html":
    case "htm":
      return (await import("@codemirror/lang-html")).html();
    case "rs":
      return (await import("@codemirror/lang-rust")).rust();
    case "py":
    case "pyi":
      return (await import("@codemirror/lang-python")).python();
    case "yaml":
    case "yml":
      return (await import("@codemirror/lang-yaml")).yaml();
    default:
      return [];
  }
}

function errorMessage(cause: unknown): string {
  return cause instanceof Error ? cause.message : String(cause);
}
