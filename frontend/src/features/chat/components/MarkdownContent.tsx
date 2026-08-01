import { useParams } from "@tanstack/react-router";
import { Check, Clipboard } from "lucide-react";
import {
  Children,
  createContext,
  isValidElement,
  memo,
  type MouseEvent as ReactMouseEvent,
  type ReactNode,
  useContext,
  useEffect,
  useState,
} from "react";
import ReactMarkdown, { defaultUrlTransform, type Components } from "react-markdown";
import remarkGfm from "remark-gfm";
import { remarkAlert } from "remark-github-blockquote-alert";

import { podFilePath, podFileUrl } from "../../../api/files.ts";
import type { files, pods, workspaces } from "../../../api/generated/index.ts";
import { Button } from "../../../components/ui/Button.tsx";
import { DiffViewer } from "../../../components/ui/DiffViewer.tsx";
import {
  podFileTarget,
  type PodFileTarget,
} from "../model/podFileLinks.ts";
import { PodFilePreviewDialog } from "./PodFilePreviewDialog.tsx";

type WorkspaceMarkdownContextValue = {
  fileTarget?: PodFileTarget;
  workspace?: workspaces.WorkspaceName;
  podId?: pods.PodId;
  openFile?: (target: PodFileTarget) => void;
};

const WorkspaceMarkdownContext = createContext<WorkspaceMarkdownContextValue>({});

export const MarkdownContent = memo(function MarkdownContent({
  content,
  density = "default",
  fileTarget,
}: {
  content: string;
  density?: "default" | "compact";
  fileTarget?: PodFileTarget;
}) {
  const params = useParams({ strict: false }) as { workspace?: string; pod?: string };
  const workspace = params.workspace as workspaces.WorkspaceName | undefined;
  const podId = params.pod as pods.PodId | undefined;
  const [previewTarget, setPreviewTarget] = useState<PodFileTarget>();

  return (
    <WorkspaceMarkdownContext.Provider
      value={{ fileTarget, workspace, podId, openFile: setPreviewTarget }}
    >
      <div
        className={
          density === "compact"
            ? "min-w-0 text-[13px] leading-[1.45] text-muted [&_h1]:my-1.5 [&_h1]:text-sm [&_h1]:font-medium [&_h2]:my-1.5 [&_h2]:text-sm [&_h2]:font-medium [&_h3]:my-1.5 [&_h3]:text-sm [&_h3]:font-medium [&_h4]:my-1.5 [&_h4]:font-medium [&_p]:my-1 [&_strong]:font-normal"
            : "min-w-0 text-[14px] leading-[1.55] text-foreground"
        }
      >
        <ReactMarkdown
          remarkPlugins={[remarkGfm, remarkAlert]}
          skipHtml
          components={markdownComponents}
          urlTransform={markdownUrlTransform}
        >
          {content}
        </ReactMarkdown>
      </div>
      {previewTarget && workspace && podId ? (
        <PodFilePreviewDialog
          key={podFilePath(previewTarget.root, previewTarget.path)}
          podId={podId}
          renderMarkdown={renderPodMarkdown}
          target={previewTarget}
          workspace={workspace}
          onClose={() => setPreviewTarget(undefined)}
        />
      ) : null}
    </WorkspaceMarkdownContext.Provider>
  );
});

const markdownComponents: Components = {
  p: ({ children, className, node: _node, ...props }) => (
    <p {...props} className={`${className ?? ""} my-2 first:mt-0 last:mb-0`}>
      {children}
    </p>
  ),
  h1: ({ children }) => <h1 className="mb-2 mt-4 text-xl font-semibold tracking-tight">{children}</h1>,
  h2: ({ children }) => <h2 className="mb-1.5 mt-4 text-lg font-semibold tracking-tight">{children}</h2>,
  h3: ({ children }) => <h3 className="mb-1.5 mt-3 text-base font-semibold">{children}</h3>,
  h4: ({ children }) => <h4 className="mb-1.5 mt-3 text-sm font-semibold">{children}</h4>,
  ul: ({ children }) => <ul className="my-2 list-disc space-y-0.5 pl-5 marker:text-subtle">{children}</ul>,
  ol: ({ children }) => <ol className="my-2 list-decimal space-y-0.5 pl-5 marker:text-subtle">{children}</ol>,
  li: ({ children }) => <li className="pl-0.5 [&>p]:my-0">{children}</li>,
  blockquote: ({ children }) => (
    <blockquote className="my-3 border-l-2 border-accent/60 bg-accent/5 py-0.5 pl-3 text-muted">
      {children}
    </blockquote>
  ),
  a: ({ children, href }) => <PodMarkdownLink href={href}>{children}</PodMarkdownLink>,
  img: ({ alt, src, title }) => <PodMarkdownImage alt={alt} src={src} title={title} />,
  hr: () => <hr className="my-4 border-ui-border" />,
  table: ({ children }) => (
    <div className="my-3 overflow-x-auto rounded-xl border border-ui-border">
      <table className="w-full border-collapse text-left text-xs">{children}</table>
    </div>
  ),
  thead: ({ children }) => <thead className="bg-surface-raised text-muted">{children}</thead>,
  th: ({ children }) => <th className="border-b border-ui-border px-3 py-2 font-semibold">{children}</th>,
  td: ({ children }) => <td className="border-b border-ui-border/70 px-3 py-2 align-top">{children}</td>,
  input: (props) => <input {...props} className="mr-2 accent-brand" disabled />,
  code: ({ children, className }) => (
    <code
      className={`${className ?? ""} rounded bg-surface-raised px-1.5 py-0.5 font-mono text-[0.9em] text-[var(--syntax-token-string)]`}
    >
      {children}
    </code>
  ),
  pre: ({ children }) => {
    const child = Children.only(children);
    if (!isValidElement<{ className?: string; children?: ReactNode }>(child)) {
      return <pre>{children}</pre>;
    }
    const language = child.props.className?.match(/language-([\w-]+)/)?.[1] ?? "text";
    const code = String(child.props.children ?? "").replace(/\n$/, "");
    return language === "diff" || language === "patch" ? (
      <DiffViewer patch={code} />
    ) : (
      <HighlightedCode code={code} language={language} />
    );
  },
};

function PodMarkdownLink({ children, href }: { children?: ReactNode; href?: string }) {
  const context = useContext(WorkspaceMarkdownContext);
  const file = podFileTarget(href, context.fileTarget);
  const openFile = context.openFile;
  const className = "font-medium text-accent-text underline decoration-accent/35 underline-offset-4 hover:text-accent";
  return file && context.workspace && context.podId && openFile ? (
    <a
      className={className}
      href={podFileUrl(
        context.workspace,
        context.podId,
        file.root,
        file.path as files.FilePath,
      )}
      rel="noreferrer"
      target="_blank"
      onClick={(event) => openFilePreview(event, file, openFile)}
    >
      {children}
    </a>
  ) : (
    <a className={className} href={href} rel="noreferrer" target="_blank">
      {children}
    </a>
  );
}

function PodMarkdownImage({
  alt,
  src,
  title,
}: {
  alt?: string;
  src?: string;
  title?: string;
}) {
  const context = useContext(WorkspaceMarkdownContext);
  const file = podFileTarget(src, context.fileTarget);
  return (
    <img
      alt={alt ?? ""}
      loading="lazy"
      src={file && context.workspace && context.podId
        ? podFileUrl(
            context.workspace,
            context.podId,
            file.root,
            file.path as files.FilePath,
          )
        : src}
      title={title}
    />
  );
}

export const HighlightedCode = memo(function HighlightedCode({
  code,
  language = "text",
}: {
  code: string;
  language?: string;
}) {
  const [html, setHtml] = useState<string>();
  const [copied, setCopied] = useState(false);

  useEffect(() => {
    let current = true;
    setHtml(undefined);
    void import("../model/highlighter.ts")
      .then(({ highlightCode }) => highlightCode(code, normalizeLanguage(language)))
      .then((value) => {
        if (current) setHtml(value);
      })
      .catch(() => {
        if (current) setHtml("");
      });
    return () => {
      current = false;
    };
  }, [code, language]);

  const copy = async () => {
    await navigator.clipboard.writeText(code);
    setCopied(true);
    window.setTimeout(() => setCopied(false), 1200);
  };

  return (
    <div className="group/code relative my-3 w-full min-w-0 max-w-full overflow-hidden rounded-xl border border-ui-border bg-[var(--syntax-background)]">
      <div className="flex h-6 items-center justify-between border-b border-ui-border px-3 font-mono text-[9px] uppercase tracking-wider text-subtle">
        <span>{language}</span>
        <Button
          className="h-5 rounded border-0 bg-transparent px-1.5 normal-case tracking-normal text-muted opacity-70 hover:bg-white/5 hover:text-foreground group-hover/code:opacity-100"
          size="small"
          onClick={() => void copy()}
        >
          {copied
            ? <Check aria-hidden="true" className="size-3" />
            : <Clipboard aria-hidden="true" className="size-3" />}
          {copied ? "Copied" : "Copy"}
        </Button>
      </div>
      {html === undefined ? (
        <pre className="overflow-x-auto px-3 py-2.5 font-mono text-xs leading-5 text-[var(--syntax-foreground)]">
          {code}
        </pre>
      ) : html ? (
        <div className="shiki-host overflow-x-auto" dangerouslySetInnerHTML={{ __html: html }} />
      ) : (
        <pre className="overflow-x-auto px-3 py-2.5 font-mono text-xs leading-5 text-[var(--syntax-foreground)]">
          {code}
        </pre>
      )}
    </div>
  );
});

function normalizeLanguage(language: string): string {
  return (
    {
      rs: "rust",
      js: "javascript",
      jsx: "jsx",
      ts: "typescript",
      py: "python",
      sh: "bash",
      shell: "bash",
      yml: "yaml",
      md: "markdown",
      plaintext: "text",
      txt: "text",
    }[language.toLowerCase()] ?? language.toLowerCase()
  );
}

function markdownUrlTransform(value: string): string {
  return podFileTarget(value) ? value : defaultUrlTransform(value);
}

function openFilePreview(
  event: ReactMouseEvent<HTMLAnchorElement>,
  target: PodFileTarget,
  openFile: (target: PodFileTarget) => void,
): void {
  if (event.button !== 0 || event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) return;
  event.preventDefault();
  openFile(target);
}

function renderPodMarkdown(
  content: string,
  root: files.FileRoot,
  path: string,
): ReactNode {
  return <MarkdownContent content={content} fileTarget={{ root, path }} />;
}
