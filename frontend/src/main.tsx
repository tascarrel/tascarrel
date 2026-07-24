import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import "@xterm/xterm/css/xterm.css";

import { AppProviders } from "./app/AppProviders.tsx";
import "./styles.css";

const root = document.getElementById("root");
if (!root) throw new Error("Missing Tascarrel application root");

createRoot(root).render(
  <StrictMode>
    <AppProviders />
  </StrictMode>,
);
