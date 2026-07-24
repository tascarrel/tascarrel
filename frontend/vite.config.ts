import { dirname } from "node:path";

import tailwindcss from "@tailwindcss/vite";
import react from "@vitejs/plugin-react";
import { defineConfig, searchForWorkspaceRoot } from "vite";

const apiTarget = process.env.TASCARREL_FRONTEND_API_TARGET ?? "http://127.0.0.1:8272";
const apiTargetUrl = new URL(apiTarget);
const routeHostnameSuffix = process.env.TASCARREL_FRONTEND_ROUTE_HOSTNAME_SUFFIX
  ?? "tascarrel.localhost";
const terminalFontPath = process.env.TASCARREL_TERMINAL_FONT_PATH;
if (!terminalFontPath) throw new Error("Missing TASCARREL_TERMINAL_FONT_PATH");

export default defineConfig({
  plugins: [react(), tailwindcss()],
  resolve: {
    alias: {
      "@tascarrel/terminal-font": terminalFontPath,
    },
  },
  server: {
    fs: {
      allow: [
        searchForWorkspaceRoot(process.cwd()),
        dirname(terminalFontPath),
      ],
    },
    port: 5174,
    proxy: {
      "^/": {
        target: apiTarget,
        ws: true,
        bypass: (request) => {
          if (isApiRequest(request.url) || isNestedHttpRoute(request.headers.host)) return;
          return request.url ?? "/";
        },
        configure: (proxy) => {
          proxy.on("proxyReq", (proxyRequest, request) => {
            if (isNestedHttpRoute(request.headers.host)) return;
            proxyRequest.setHeader("host", apiTargetUrl.host);
          });
          proxy.on("proxyReqWs", (proxyRequest, request) => {
            if (isNestedHttpRoute(request.headers.host)) return;
            proxyRequest.setHeader("host", apiTargetUrl.host);
            if (request.headers.origin) {
              proxyRequest.setHeader("origin", apiTargetUrl.origin);
            }
          });
        },
      },
    },
  },
});

function isApiRequest(path: string | undefined): boolean {
  return path === "/api" || path?.startsWith("/api/") === true;
}

function isNestedHttpRoute(authority: string | undefined): boolean {
  if (!authority) return false;
  try {
    const hostname = new URL(`http://${authority}`).hostname.toLowerCase();
    return hostname.endsWith(`.${routeHostnameSuffix.toLowerCase()}`);
  } catch {
    return false;
  }
}
