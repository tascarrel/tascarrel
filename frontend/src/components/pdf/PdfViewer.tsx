import { ChevronLeft, ChevronRight, LoaderCircle, ZoomIn, ZoomOut } from "lucide-react";
import { useEffect, useLayoutEffect, useRef, useState } from "react";
import type {
  PDFDocumentLoadingTask,
  PDFDocumentProxy,
  RenderTask,
} from "pdfjs-dist";
import pdfWorkerUrl from "pdfjs-dist/build/pdf.worker.min.mjs?url";

import { Button } from "../ui/Button.tsx";

type LoadState =
  | { state: "loading" }
  | { state: "ready"; document: PDFDocumentProxy }
  | { state: "error"; message: string };

/** A reusable, single-page PDF.js canvas viewer. */
export function PdfViewer({
  source,
  className = "",
}: {
  source: string | URL;
  className?: string;
}) {
  const [load, setLoad] = useState<LoadState>({ state: "loading" });
  const [pageNumber, setPageNumber] = useState(1);
  const [zoom, setZoom] = useState(1);
  const [rendering, setRendering] = useState(false);
  const [viewportWidth, setViewportWidth] = useState(0);
  const canvas = useRef<HTMLCanvasElement>(null);
  const viewport = useRef<HTMLDivElement>(null);

  useEffect(() => {
    let active = true;
    let loadingTask: PDFDocumentLoadingTask | undefined;
    setLoad({ state: "loading" });
    setPageNumber(1);
    setZoom(1);

    void import("pdfjs-dist")
      .then((pdfjs) => {
        if (!active) return undefined;
        pdfjs.GlobalWorkerOptions.workerSrc = pdfWorkerUrl;
        loadingTask = pdfjs.getDocument({
          url: String(source),
          // Keep the reusable viewer self-contained instead of requiring a
          // separately hosted PDF.js WASM asset tree.
          useWasm: false,
        });
        return loadingTask.promise;
      })
      .then((loadedDocument) => {
        if (!loadedDocument) return;
        if (active) setLoad({ state: "ready", document: loadedDocument });
      })
      .catch((cause: unknown) => {
        if (!active) return;
        setLoad({
          state: "error",
          message: cause instanceof Error ? cause.message : "The PDF could not be opened.",
        });
      });

    return () => {
      active = false;
      void loadingTask?.destroy();
    };
  }, [source]);

  useLayoutEffect(() => {
    const element = viewport.current;
    if (!element) return;
    const update = () => setViewportWidth(element.clientWidth);
    update();
    const observer = new ResizeObserver(update);
    observer.observe(element);
    return () => observer.disconnect();
  }, []);

  useEffect(() => {
    if (load.state !== "ready" || viewportWidth === 0) return;
    let active = true;
    let renderTask: RenderTask | undefined;
    setRendering(true);

    void load.document
      .getPage(pageNumber)
      .then((page) => {
        if (!active || !canvas.current) return;
        const naturalViewport = page.getViewport({ scale: 1 });
        const fitScale = Math.max(0.1, (viewportWidth - 32) / naturalViewport.width);
        const pageViewport = page.getViewport({ scale: fitScale * zoom });
        const outputScale = Math.max(1, window.devicePixelRatio || 1);
        const target = canvas.current;
        target.width = Math.floor(pageViewport.width * outputScale);
        target.height = Math.floor(pageViewport.height * outputScale);
        target.style.width = `${Math.floor(pageViewport.width)}px`;
        target.style.height = `${Math.floor(pageViewport.height)}px`;
        renderTask = page.render({
          canvas: target,
          viewport: pageViewport,
          transform: outputScale === 1 ? undefined : [outputScale, 0, 0, outputScale, 0, 0],
        });
        return renderTask.promise;
      })
      .then(() => {
        if (active) setRendering(false);
      })
      .catch((cause: unknown) => {
        if (active && !isRenderCancellation(cause)) {
          setLoad({
            state: "error",
            message: cause instanceof Error ? cause.message : "The PDF page could not be rendered.",
          });
        }
      });

    return () => {
      active = false;
      renderTask?.cancel();
    };
  }, [load, pageNumber, viewportWidth, zoom]);

  const pageCount = load.state === "ready" ? load.document.numPages : 0;
  return (
    <div className={`flex min-h-0 flex-1 flex-col bg-canvas ${className}`}>
      <div className="flex h-11 shrink-0 items-center justify-between gap-3 border-b border-ui-border px-3">
        <div className="flex items-center gap-1">
          <Button
            aria-label="Previous page"
            className="size-7 border-0 bg-transparent p-0"
            size="icon"
            disabled={load.state !== "ready" || pageNumber <= 1}
            onClick={() => setPageNumber((current) => Math.max(1, current - 1))}
          >
            <ChevronLeft className="size-3.5" />
          </Button>
          <span
            aria-live="polite"
            className="min-w-20 text-center text-xs tabular-nums text-muted"
          >
            {pageCount > 0 ? `${pageNumber} / ${pageCount}` : "—"}
          </span>
          <Button
            aria-label="Next page"
            className="size-7 border-0 bg-transparent p-0"
            size="icon"
            disabled={load.state !== "ready" || pageNumber >= pageCount}
            onClick={() => setPageNumber((current) => Math.min(pageCount, current + 1))}
          >
            <ChevronRight className="size-3.5" />
          </Button>
        </div>
        <div className="flex items-center gap-1">
          <Button
            aria-label="Zoom out"
            className="size-7 border-0 bg-transparent p-0"
            size="icon"
            disabled={load.state !== "ready" || zoom <= 0.5}
            onClick={() => setZoom((current) => Math.max(0.5, current - 0.25))}
          >
            <ZoomOut className="size-3.5" />
          </Button>
          <span className="min-w-12 text-center text-[10px] tabular-nums text-subtle">
            {Math.round(zoom * 100)}%
          </span>
          <Button
            aria-label="Zoom in"
            className="size-7 border-0 bg-transparent p-0"
            size="icon"
            disabled={load.state !== "ready" || zoom >= 3}
            onClick={() => setZoom((current) => Math.min(3, current + 0.25))}
          >
            <ZoomIn className="size-3.5" />
          </Button>
        </div>
      </div>
      <div
        className="relative min-h-0 flex-1 overflow-auto bg-black/30 p-4"
        ref={viewport}
      >
        {load.state === "error" ? (
          <div
            className="grid min-h-full place-items-center px-8 text-center text-sm text-red-200"
            role="alert"
          >
            {load.message}
          </div>
        ) : (
          <div className="flex min-h-full min-w-full items-start justify-center">
            <canvas
              aria-label={`PDF page ${pageNumber}`}
              className={`bg-white shadow-2xl shadow-black/50 transition-opacity ${
                load.state === "loading" || rendering ? "opacity-45" : "opacity-100"
              }`}
              ref={canvas}
              role="img"
            />
          </div>
        )}
        {load.state === "loading" || rendering ? (
          <div
            className="pointer-events-none absolute inset-0 grid place-items-center"
            role="status"
          >
            <LoaderCircle aria-hidden="true" className="size-5 animate-spin text-muted" />
            <span className="sr-only">
              {load.state === "loading" ? "Loading PDF" : `Rendering PDF page ${pageNumber}`}
            </span>
          </div>
        ) : null}
      </div>
    </div>
  );
}

function isRenderCancellation(cause: unknown): boolean {
  return cause instanceof Error && cause.name === "RenderingCancelledException";
}
