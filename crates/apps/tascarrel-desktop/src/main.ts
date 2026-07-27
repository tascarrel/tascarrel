import { spawn } from "node:child_process";
import { accessSync, constants, readFileSync } from "node:fs";
import { homedir } from "node:os";
import { delimiter, isAbsolute, join } from "node:path";
import { connect } from "node:net";

import { app, BrowserWindow, dialog, shell } from "electron";

const SERVER_ADDRESS = "127.0.0.1";
const SERVER_PORT = 8272;
const SERVER_PROBE_INTERVAL_MS = 50;
const SERVER_PROBE_TIMEOUT_MS = 250;
const SERVER_START_TIMEOUT_MS = 30_000;
const APPLICATION_HOSTNAME = "tascarrel.localhost";
const APPLICATION_ORIGIN = `http://${APPLICATION_HOSTNAME}:${SERVER_PORT}`;
const protocolVersion = Number(
  readFileSync(join(__dirname, "protocol-version"), "utf8").trim(),
);

app.commandLine.appendSwitch(
  "host-resolver-rules",
  `MAP ${APPLICATION_HOSTNAME} ${SERVER_ADDRESS}, MAP *.${APPLICATION_HOSTNAME} ${SERVER_ADDRESS}`,
);
if (process.platform === "linux") app.commandLine.appendSwitch("class", "Tascarrel");

let mainWindow: BrowserWindow | undefined;
let applicationStarted: Promise<void> | undefined;

if (!app.requestSingleInstanceLock()) {
  app.quit();
} else {
  app.on("second-instance", () => {
    void startApplication().then(showMainWindow).catch(reportFatalError);
  });
  app.on("activate", () => {
    void startApplication().then(showMainWindow).catch(reportFatalError);
  });
  app.on("window-all-closed", () => {
    if (process.platform !== "darwin") app.quit();
  });
  void app.whenReady().then(startApplication).then(showMainWindow).catch(reportFatalError);
}

function startApplication(): Promise<void> {
  applicationStarted ??= ensureServer();
  return applicationStarted;
}

async function showMainWindow(): Promise<void> {
  if (mainWindow) {
    if (mainWindow.isMinimized()) mainWindow.restore();
    mainWindow.show();
    mainWindow.focus();
    return;
  }

  const window = new BrowserWindow({
    autoHideMenuBar: true,
    backgroundColor: "#111416",
    height: 900,
    icon: process.platform === "linux" ? join(__dirname, "../icons/icon.png") : undefined,
    minHeight: 600,
    minWidth: 900,
    show: false,
    title: "Tascarrel",
    webPreferences: {
      contextIsolation: true,
      nodeIntegration: false,
      sandbox: true,
      webSecurity: true,
    },
    width: 1440,
  });
  mainWindow = window;
  window.on("closed", () => {
    if (mainWindow === window) mainWindow = undefined;
  });
  window.webContents.setWindowOpenHandler(({ url }) => {
    if (url.startsWith("http://") || url.startsWith("https://")) {
      void shell.openExternal(url);
    }
    return { action: "deny" };
  });
  window.webContents.on("will-navigate", (event, url) => {
    if (isApplicationUrl(url)) return;
    event.preventDefault();
    if (url.startsWith("http://") || url.startsWith("https://")) {
      void shell.openExternal(url);
    }
  });
  window.once("ready-to-show", () => window.show());

  const startupUrl = new URL("/startup", APPLICATION_ORIGIN);
  startupUrl.searchParams.set("desktopVersion", app.getVersion());
  startupUrl.searchParams.set("desktopProtocolVersion", String(protocolVersion));
  try {
    await window.loadURL(startupUrl.toString());
    if (!window.isVisible()) window.show();
  } catch (cause) {
    window.destroy();
    throw new Error("Failed to load the Tascarrel interface", { cause });
  }
}

async function ensureServer(): Promise<void> {
  if (await serverIsListening()) return;

  const child = spawn(serverExecutable(), [], {
    detached: true,
    env: {
      ...process.env,
      PATH: graphicalApplicationPath(),
      TASCARREL_HOME: tascarrelHome(),
    },
    stdio: "ignore",
  });
  child.unref();

  let stopped: Error | undefined;
  child.once("error", (cause) => {
    stopped = new Error("Failed to start the bundled Tascarrel server", { cause });
  });
  child.once("exit", (code, signal) => {
    stopped = new Error(
      `Bundled Tascarrel server stopped before listening (${signal ?? `exit code ${code ?? "unknown"}`})`,
    );
  });

  const deadline = Date.now() + SERVER_START_TIMEOUT_MS;
  while (!(await serverIsListening())) {
    if (stopped) throw stopped;
    if (Date.now() >= deadline) {
      throw new Error("Bundled Tascarrel server did not listen within 30 seconds");
    }
    await new Promise((resolve) => setTimeout(resolve, SERVER_PROBE_INTERVAL_MS));
  }
}

function serverIsListening(): Promise<boolean> {
  return new Promise((resolve) => {
    const socket = connect({ host: SERVER_ADDRESS, port: SERVER_PORT });
    let settled = false;
    const finish = (listening: boolean) => {
      if (settled) return;
      settled = true;
      socket.destroy();
      resolve(listening);
    };
    socket.setTimeout(SERVER_PROBE_TIMEOUT_MS);
    socket.once("connect", () => finish(true));
    socket.once("error", () => finish(false));
    socket.once("timeout", () => finish(false));
  });
}

function serverExecutable(): string {
  const configured = process.env.TASCARREL_DESKTOP_SERVER;
  const executable = configured || join(process.resourcesPath, "tascarrel");
  if (!isAbsolute(executable)) {
    throw new Error("TASCARREL_DESKTOP_SERVER must be an absolute path");
  }
  try {
    accessSync(executable, constants.X_OK);
  } catch (cause) {
    throw new Error(`Tascarrel server is not executable: ${executable}`, { cause });
  }
  return executable;
}

function tascarrelHome(): string {
  const configured = process.env.TASCARREL_HOME;
  if (configured) {
    if (!isAbsolute(configured)) {
      throw new Error("TASCARREL_HOME must be an absolute path");
    }
    return configured;
  }
  const home = homedir();
  if (!isAbsolute(home)) {
    throw new Error("The current user's home directory must be an absolute path");
  }
  return join(home, ".tascarrel");
}

function graphicalApplicationPath(): string {
  const paths = (process.env.PATH ?? "").split(delimiter).filter(Boolean);
  for (const directory of [
    "/opt/homebrew/bin",
    "/usr/local/bin",
    "/opt/local/bin",
    "/usr/bin",
    "/bin",
  ]) {
    if (!paths.includes(directory)) paths.push(directory);
  }
  return paths.join(delimiter);
}

function isApplicationUrl(value: string): boolean {
  try {
    const url = new URL(value);
    return url.protocol === "http:"
      && url.hostname === APPLICATION_HOSTNAME
      && url.port === String(SERVER_PORT);
  } catch {
    return false;
  }
}

function reportFatalError(cause: unknown): void {
  const error = cause instanceof Error ? cause : new Error(String(cause));
  console.error(error);
  if (app.isReady()) {
    dialog.showErrorBox("Tascarrel Could Not Open", error.message);
  }
  app.exit(1);
}
