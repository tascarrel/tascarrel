import { ExternalLink, Monitor, RefreshCw } from "lucide-react";
import { useEffect, useMemo, useState, type FormEvent } from "react";

import { Button } from "../../../components/ui/Button.tsx";
import type { IframeFrameSpec } from "../../../components/ui/IframePool.tsx";
import { ShellPlaceholder } from "./ShellPlaceholder.tsx";
import { useWebPreviewFrame } from "./WebPreviewFramePool.tsx";

export type WebPreview = {
  id: string;
  title: string;
  url: string;
};

export function WebPreviewView({
  frameId,
  preview,
  revision,
  onNavigate,
  onReload,
}: {
  frameId: string;
  preview: WebPreview;
  revision: number;
  onNavigate: (address: string) => void;
  onReload: () => void;
}) {
  const [address, setAddress] = useState(preview.url);
  const [frameAnchor, setFrameAnchor] = useState<HTMLDivElement | null>(null);
  const frame = useMemo<IframeFrameSpec | undefined>(() => preview.url ? {
    id: frameId,
    src: preview.url,
    title: `${preview.title} preview`,
    revision,
    background: "document",
    iframeProps: { referrerPolicy: "strict-origin-when-cross-origin" },
  } : undefined, [frameId, preview.title, preview.url, revision]);
  useWebPreviewFrame(frame, frameAnchor);
  useEffect(() => setAddress(preview.url), [preview.id, preview.url]);
  const submitAddress = (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    onNavigate(address);
  };
  return (
    <div className="web-preview" role="region" aria-label={`${preview.title} web preview`}>
      <div className="web-preview-toolbar">
        <form className="web-preview-address" onSubmit={submitAddress}>
          <input
            aria-label="Preview address"
            autoCapitalize="none"
            autoComplete="off"
            spellCheck="false"
            placeholder="Enter a URL"
            value={address}
            onChange={(event) => setAddress(event.target.value)}
          />
        </form>
        <Button
          aria-label={`Reload ${preview.title}`}
          className="web-preview-toolbar-button rounded-none border-0 bg-transparent p-0"
          size="icon"
          title="Reload preview"
          disabled={!preview.url}
          onClick={onReload}
        >
          <RefreshCw aria-hidden="true" size={12} />
        </Button>
        {preview.url ? (
          <a
            className="web-preview-toolbar-button"
            href={preview.url}
            target="_blank"
            rel="noreferrer"
            aria-label={`Open ${preview.title} in a new tab`}
            title="Open in new tab"
          >
            <ExternalLink aria-hidden="true" size={12} />
          </a>
        ) : null}
      </div>
      <div
        ref={setFrameAnchor}
        className={`web-preview-frame-host ${preview.url ? "" : "web-preview-frame-host-empty"}`}
      >
        {!preview.url ? (
          <ShellPlaceholder
            icon={Monitor}
            title="New web preview"
            detail="Enter an address above to load a site."
          />
        ) : null}
      </div>
    </div>
  );
}

export function normalizePreviewUrl(address: string): string | undefined {
  const trimmed = address.trim();
  if (!trimmed) return undefined;
  const candidate = /^https?:\/\//i.test(trimmed) ? trimmed : `https://${trimmed}`;
  try {
    const url = new URL(candidate);
    return url.protocol === "http:" || url.protocol === "https:" ? url.toString() : undefined;
  } catch {
    return undefined;
  }
}

export function previewTitleForUrl(url: string): string {
  try {
    return new URL(url).hostname.replace(/^www\./, "") || "Web preview";
  } catch {
    return "Web preview";
  }
}
