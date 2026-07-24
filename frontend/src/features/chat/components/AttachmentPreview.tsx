import { Dialog } from "@base-ui/react/dialog";
import { File, FileText, X } from "lucide-react";
import { useState } from "react";

import type { chats } from "../../../api/generated/index.ts";
import { PdfViewer } from "../../../components/pdf/index.ts";
import { Button } from "../../../components/ui/Button.tsx";
import { formatBytes } from "../model/format.ts";

export type PreviewAttachment = {
  attachmentId?: chats.ChatAttachmentId;
  name: string;
  mediaType?: string;
  size?: number;
};

export function AttachmentPreview({
  attachment,
  url,
  removable = false,
  onRemove,
}: {
  attachment: PreviewAttachment;
  url?: string;
  removable?: boolean;
  onRemove?: () => void;
}) {
  const [open, setOpen] = useState(false);
  const [thumbnailFailed, setThumbnailFailed] = useState(false);
  const image = isSafeRasterImage(attachment.mediaType);
  const pdf = attachment.mediaType?.toLowerCase() === "application/pdf";
  const previewable = Boolean(url) && (image || pdf);
  const details = [
    attachment.mediaType,
    attachment.size === undefined ? undefined : formatBytes(attachment.size),
  ].filter((detail): detail is string => detail !== undefined);
  const preview = (
    <div className="grid size-11 shrink-0 place-items-center overflow-hidden rounded-lg border border-ui-border bg-black/25">
      {image && url && !thumbnailFailed ? (
        <img
          alt=""
          className="size-full object-cover"
          loading="lazy"
          src={url}
          onError={() => setThumbnailFailed(true)}
        />
      ) : pdf ? (
        <FileText className="size-4 text-red-300" />
      ) : (
        <File className="size-4 text-muted" />
      )}
    </div>
  );

  return (
    <>
      <div className="flex min-w-48 max-w-full items-center gap-2 rounded-xl border border-ui-border bg-surface p-1.5 pr-2 text-xs">
        {previewable ? (
          <button
            aria-label={`Open ${attachment.name}`}
            className="flex min-w-0 flex-1 items-center gap-2 rounded-lg text-left outline-none transition hover:bg-surface-raised focus-visible:outline-2 focus-visible:outline-accent"
            type="button"
            onClick={() => setOpen(true)}
          >
            {preview}
            <AttachmentDetails name={attachment.name} details={details} />
          </button>
        ) : (
          <div className="flex min-w-0 flex-1 items-center gap-2">
            {preview}
            <AttachmentDetails name={attachment.name} details={details} />
          </div>
        )}
        {removable && onRemove ? (
          <Button
            aria-label={`Remove ${attachment.name}`}
            className="size-7 shrink-0 border-0 bg-transparent p-0"
            size="icon"
            title="Remove attachment"
            onClick={onRemove}
          >
            <X className="size-3.5" />
          </Button>
        ) : null}
      </div>

      {previewable && url ? (
        <Dialog.Root open={open} onOpenChange={setOpen}>
          <Dialog.Portal>
            <Dialog.Backdrop className="fixed inset-0 z-50 bg-black/85 backdrop-blur-sm transition-opacity data-[ending-style]:opacity-0 data-[starting-style]:opacity-0" />
            <Dialog.Viewport className="fixed inset-0 z-50 grid place-items-center overflow-hidden p-3 sm:p-6">
              <Dialog.Popup className="flex max-h-full w-full max-w-6xl flex-col overflow-hidden rounded-2xl border border-ui-border-strong bg-surface-raised text-foreground shadow-2xl shadow-black/70 outline-none transition-[transform,opacity] data-[ending-style]:scale-[0.98] data-[ending-style]:opacity-0 data-[starting-style]:scale-[0.98] data-[starting-style]:opacity-0">
                <div className="flex h-12 shrink-0 items-center gap-3 border-b border-ui-border px-4">
                  <Dialog.Title className="min-w-0 flex-1 truncate text-sm font-medium">
                    {attachment.name}
                  </Dialog.Title>
                  <Dialog.Close
                    aria-label="Close preview"
                    className="grid size-8 shrink-0 place-items-center rounded-lg text-muted outline-none transition hover:bg-surface hover:text-foreground focus-visible:outline-2 focus-visible:outline-accent"
                  >
                    <X className="size-4" />
                  </Dialog.Close>
                </div>
                {pdf ? (
                  <PdfViewer className="h-[min(80vh,56rem)]" source={url} />
                ) : (
                  <div className="grid min-h-0 flex-1 place-items-center overflow-auto bg-black/40 p-4">
                    <img
                      alt={attachment.name}
                      className="max-h-[calc(100vh-8rem)] max-w-full object-contain"
                      src={url}
                    />
                  </div>
                )}
              </Dialog.Popup>
            </Dialog.Viewport>
          </Dialog.Portal>
        </Dialog.Root>
      ) : null}
    </>
  );
}

function AttachmentDetails({ name, details }: { name: string; details: string[] }) {
  return (
    <span className="min-w-0 flex-1 pr-1">
      <span className="block truncate font-medium text-foreground">{name}</span>
      {details.length > 0 ? (
        <span className="mt-0.5 block truncate text-[10px] text-subtle">
          {details.join(" · ")}
        </span>
      ) : null}
    </span>
  );
}

const SAFE_RASTER_IMAGE_TYPES = new Set([
  "image/avif",
  "image/bmp",
  "image/gif",
  "image/jpeg",
  "image/png",
  "image/webp",
  "image/x-icon",
]);

/** Restricts previews to passive raster formats rather than active image content. */
function isSafeRasterImage(mediaType?: string): boolean {
  return mediaType !== undefined && SAFE_RASTER_IMAGE_TYPES.has(mediaType.toLowerCase());
}
