import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import "@xterm/xterm/css/xterm.css";

import { AuthBootstrap } from "./app/AuthBootstrap.tsx";
import "./styles.css";

const root = document.getElementById("root");
if (!root) throw new Error("Missing Tascarrel application root");

const application = createRoot(root);
application.render(
  <StrictMode>
    <AuthBootstrap
      onAuthenticated={async () => {
        const { AppProviders } = await import("./app/AppProviders.tsx");
        application.render(
          <StrictMode>
            <AppProviders />
          </StrictMode>,
        );
      }}
    />
  </StrictMode>,
);
