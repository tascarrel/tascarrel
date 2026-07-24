import { RouterProvider } from "@tanstack/react-router";
import { useState } from "react";

import { CodeFramePoolProvider } from "../features/code/CodeFramePool.tsx";
import { GlobalCommandPaletteProvider } from "../components/ui/GlobalCommandPalette.tsx";
import { WebPreviewFramePoolProvider } from "../features/workbench/shell/WebPreviewFramePool.tsx";
import { BackendStateCache } from "../shared/state/BackendStateCache.ts";
import { StateCacheProvider } from "../shared/state/StateCacheProvider.tsx";
import { router } from "./router.tsx";

export function AppProviders() {
  const [stateCache] = useState(() => new BackendStateCache());
  return (
    <StateCacheProvider cache={stateCache}>
      <GlobalCommandPaletteProvider>
        <CodeFramePoolProvider>
          <WebPreviewFramePoolProvider>
            <RouterProvider router={router} />
          </WebPreviewFramePoolProvider>
        </CodeFramePoolProvider>
      </GlobalCommandPaletteProvider>
    </StateCacheProvider>
  );
}
