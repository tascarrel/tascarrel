import type { ReactNode } from "react";

import {
  createIframePool,
  type IframeFrameSpec,
  IframePoolProvider,
  useIframeFrame,
  useRetainedIframeFrames,
} from "../../../components/ui/IframePool.tsx";

const WEB_PREVIEW_FRAME_POOL = createIframePool();

/** Retains web-preview iframes across workbench view and route changes. */
export function WebPreviewFramePoolProvider({ children }: { children: ReactNode }) {
  return (
    <IframePoolProvider pool={WEB_PREVIEW_FRAME_POOL}>
      {children}
    </IframePoolProvider>
  );
}

/** Activates a web-preview iframe in the global web-preview frame pool. */
export function useWebPreviewFrame(
  spec: IframeFrameSpec | undefined,
  anchor: HTMLElement | null,
) {
  useIframeFrame(WEB_PREVIEW_FRAME_POOL, spec, anchor);
}

/** Synchronizes retained web-preview frames with the currently available preview tabs. */
export function useRetainedWebPreviewFrames(frameIds: readonly string[]) {
  useRetainedIframeFrames(WEB_PREVIEW_FRAME_POOL, frameIds);
}

/** Creates a globally unique pool ID for one workspace preview. */
export function webPreviewFrameId(workspace: string, previewId: string): string {
  return JSON.stringify([workspace, previewId]);
}
