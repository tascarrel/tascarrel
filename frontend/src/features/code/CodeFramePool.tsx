import type { ReactNode } from "react";

import {
  createIframePool,
  type IframeFrameSpec,
  IframePoolProvider,
  useIframeFrame,
} from "../../components/ui/IframePool.tsx";

const CODE_FRAME_POOL = createIframePool();

/** Retains a small global LRU of code-server iframes across route and workspace changes. */
export function CodeFramePoolProvider({ children }: { children: ReactNode }) {
  return (
    <IframePoolProvider maxFrames={4} pool={CODE_FRAME_POOL}>
      {children}
    </IframePoolProvider>
  );
}

/** Activates a code-server iframe in the global code frame pool. */
export function useCodeFrame(
  spec: IframeFrameSpec | undefined,
  anchor: HTMLElement | null,
) {
  useIframeFrame(CODE_FRAME_POOL, spec, anchor);
}
