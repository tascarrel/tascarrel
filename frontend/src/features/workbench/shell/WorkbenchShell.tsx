import { useNavigate } from "@tanstack/react-router";
import {
  Bot,
  Box,
  Code2,
  Files,
  GitPullRequest,
  GitBranch,
  Images,
  LoaderCircle,
  Monitor,
  Network,
  Settings,
  TerminalSquare,
} from "lucide-react";
import {
  useEffect,
  useMemo,
  useRef,
  useState,
  type ReactNode,
} from "react";

import type { pods, workspaces } from "../../../api/generated/index.ts";
import { ConnectionOverlay } from "../../../components/ui/ConnectionOverlay.tsx";
import {
  type GlobalCommandDefinition,
  useGlobalCommands,
} from "../../../components/ui/GlobalCommandPalette.tsx";
import type { PodChangeSummary } from "../../changes/podChangeSummary.ts";
import { PodOverview } from "./PodOverview.tsx";
import { ResizableDock } from "./ResizableDock.tsx";
import { ShellPlaceholder } from "./ShellPlaceholder.tsx";
import { ShellPanelToggle } from "./ShellPanelToggle.tsx";
import {
  ShellTab,
  ShellTabAction,
  ShellTabStrip,
} from "./ShellTabBar.tsx";
import {
  ShellModeNav,
  WorkbenchModeNav,
  type ShellModeOption,
  type WorkbenchMode,
} from "./WorkbenchModeNav.tsx";
import {
  normalizePreviewUrl,
  previewTitleForUrl,
  type WebPreview,
  WebPreviewView,
} from "./WebPreview.tsx";
import {
  useRetainedWebPreviewFrames,
  webPreviewFrameId,
} from "./WebPreviewFramePool.tsx";
import {
  WorkspaceSidebar,
  type WorkspaceControlView,
} from "./WorkspaceSidebar.tsx";

type WorkbenchShellProps = {
  workspaces: readonly workspaces.Workspace[];
  selectedWorkspace?: workspaces.WorkspaceName;
  usbEnabled: boolean;
  pods: readonly pods.Pod[];
  podChangeSummaries: ReadonlyMap<pods.PodId, PodChangeSummary>;
  podChangeSummariesVerified: boolean;
  selectedPodId?: pods.PodId;
  view: WorkbenchMode;
  workspaceConnection: "idle" | "connecting" | "live" | "reconnecting";
  workspaceConnectionAttempt: number;
  workspaceScreen?: ReactNode;
  podListEmptyMessage?: string;
  showPodCount?: boolean;
  onSelectWorkspace: (workspace: workspaces.WorkspaceName) => void;
  onCreateWorkspace: (workspace: workspaces.WorkspaceName) => Promise<void>;
  onStartWorkspace: (workspace: workspaces.WorkspaceName) => Promise<void>;
  onStopWorkspace: (workspace: workspaces.WorkspaceName) => Promise<void>;
  onSelectPod: (podId: pods.PodId) => void;
  canCreatePod: boolean;
  podCreationActive?: boolean;
  onCreatePod: () => void;
  onStartPod: (podId: pods.PodId) => Promise<void>;
  onStopPod: (podId: pods.PodId) => Promise<void>;
  onDestroyPod: (podId: pods.PodId) => Promise<void>;
  agentView: ReactNode;
  codeView: ReactNode;
  changesView: ReactNode;
  filesView: ReactNode;
  codeTabs: CodeWorkbenchTab[];
  selectedCodeFolder?: string;
  onSelectCodeSession: (folder: string) => void;
  onNewCodeSession: () => void;
  onCloseCodeSession: (folder: string) => void;
  imagesView: ReactNode;
  networkView: ReactNode;
  repositoriesView: ReactNode;
  settingsView: ReactNode;
  publishedWebPreviews?: readonly WebPreview[];
  podProcessView?: ReactNode;
  agentTabs: AgentWorkbenchTab[];
  selectedAgentId?: string;
  creatingAgent?: boolean;
  busyPodIds?: ReadonlySet<pods.PodId>;
  attentionPodIds?: ReadonlySet<pods.PodId>;
  agentNeedsInput?: boolean;
  repositoryApprovalCount?: number;
  onSelectAgent: (agentId: string) => void;
  onNewAgent: () => void;
  onArchiveAgent: (agentId: string) => void;
  terminalView: ReactNode;
  terminalTabs: TerminalWorkbenchTab[];
  terminalTabsReady: boolean;
  activeTerminalId?: string;
  onSelectTerminal: (terminalId: string) => void;
  onNewTerminal: () => void;
  onCloseTerminal: (terminalId: string) => void;
};

export type AgentWorkbenchTab = {
  id: string;
  title: string;
  status?: "working" | "needs-input" | "failed" | "connected";
  attention: boolean;
};

export type CodeWorkbenchTab = {
  folder: string;
  title: string;
  status: "starting" | "running" | "exited" | "failed";
  closeable: boolean;
};

export type TerminalWorkbenchTab = {
  id: string;
  title: string;
  status: "running" | "exited" | "failed";
};

type LayoutState = {
  sidebarOpen: boolean;
  terminalOpen: boolean;
  terminalSize: number;
  previewOpen: boolean;
  previewSize: number;
};

type StoredWorkbenchLayouts = {
  defaultLayout: LayoutState;
  layoutsByTarget: Record<string, LayoutState>;
};

const LAYOUT_STORAGE_KEY = "tascarrel.web-chat.shell.layout.v2";
const LEGACY_LAYOUT_STORAGE_KEY = "tascarrel.web-chat.shell.layout.v1";
const MAX_PREVIEW_SIZE = 10_000;
const SIDEBAR_SHORTCUT = ["Mod", "B"] as const;
const PREVIEW_SHORTCUT = ["Mod", "Alt", "B"] as const;
const TERMINAL_SHORTCUT = ["Mod", "J"] as const;
const DEFAULT_LAYOUT: LayoutState = {
  sidebarOpen: true,
  terminalOpen: false,
  terminalSize: 280,
  previewOpen: false,
  previewSize: 440,
};
const INITIAL_WEB_PREVIEWS: WebPreview[] = [];
const EMPTY_POD_IDS: ReadonlySet<pods.PodId> = new Set();
const WEB_PANEL_MODES = [
  { value: "web", label: "Web", icon: Monitor },
] satisfies Array<ShellModeOption<"web">>;
const TERMINAL_PANEL_MODES = [
  { value: "terminal", label: "Terminals", icon: TerminalSquare },
] satisfies Array<ShellModeOption<"terminal">>;
const MODE_PRESENTATION = {
  agent: { label: "Agent", icon: Bot },
  code: { label: "Code", icon: Code2 },
  changes: { label: "Changes", icon: GitPullRequest },
  files: { label: "Files", icon: Files },
  pod: { label: "Pod", icon: Box },
  images: { label: "Images", icon: Images },
  network: { label: "Network", icon: Network },
  repositories: { label: "Repositories", icon: GitBranch },
  settings: { label: "Settings", icon: Settings },
} satisfies Record<WorkbenchMode, { label: string; icon: typeof Bot }>;
const POD_WORKBENCH_MODES = new Set<WorkbenchMode>([
  "agent",
  "code",
  "changes",
  "files",
  "pod",
]);

export function WorkbenchShell({
  workspaces,
  selectedWorkspace,
  usbEnabled,
  pods,
  podChangeSummaries,
  podChangeSummariesVerified,
  selectedPodId,
  view,
  workspaceConnection,
  workspaceConnectionAttempt,
  workspaceScreen,
  podListEmptyMessage,
  showPodCount = true,
  onSelectWorkspace,
  onCreateWorkspace,
  onStartWorkspace,
  onStopWorkspace,
  onSelectPod,
  canCreatePod,
  podCreationActive = false,
  onCreatePod,
  onStartPod,
  onStopPod,
  onDestroyPod,
  agentView,
  codeView,
  changesView,
  filesView,
  codeTabs,
  selectedCodeFolder,
  onSelectCodeSession,
  onNewCodeSession,
  onCloseCodeSession,
  imagesView,
  networkView,
  repositoriesView,
  settingsView,
  publishedWebPreviews = INITIAL_WEB_PREVIEWS,
  podProcessView,
  agentTabs,
  selectedAgentId,
  creatingAgent = false,
  busyPodIds = EMPTY_POD_IDS,
  attentionPodIds = EMPTY_POD_IDS,
  agentNeedsInput = false,
  repositoryApprovalCount,
  onSelectAgent,
  onNewAgent,
  onArchiveAgent,
  terminalView,
  terminalTabs,
  terminalTabsReady,
  activeTerminalId,
  onSelectTerminal,
  onNewTerminal,
  onCloseTerminal,
}: WorkbenchShellProps) {
  const navigate = useNavigate();
  const mode = view;
  const [storedLayouts, setStoredLayouts] = useState<StoredWorkbenchLayouts>(
    loadStoredWorkbenchLayouts,
  );
  const layoutTarget = selectedWorkspace
    ? JSON.stringify([selectedWorkspace, selectedPodId ?? null])
    : undefined;
  const layout = layoutTarget
    ? storedLayouts.layoutsByTarget[layoutTarget] ?? storedLayouts.defaultLayout
    : storedLayouts.defaultLayout;
  const terminalPanelTarget = layoutTarget ?? "default";
  const handledTerminalPanelTarget = useRef<string | undefined>(undefined);
  const [webPreviews, setWebPreviews] = useState(INITIAL_WEB_PREVIEWS);
  const [publishedPreviewUrls, setPublishedPreviewUrls] = useState<Record<string, string>>({});
  const [activeWebPreviewId, setActiveWebPreviewId] = useState<string>();
  const [webPreviewRevisions, setWebPreviewRevisions] = useState<Record<string, number>>({});
  const nextWebPreviewNumber = useRef(1);
  const presentation = MODE_PRESENTATION[mode];
  const publishedWebPreviewIds = useMemo(
    () => new Set(publishedWebPreviews.map((preview) => preview.id)),
    [publishedWebPreviews],
  );
  const availableWebPreviews = useMemo(() => [
    ...publishedWebPreviews.map((preview) => ({
      ...preview,
      url: publishedPreviewUrls[preview.id] ?? preview.url,
    })),
    ...webPreviews,
  ], [publishedPreviewUrls, publishedWebPreviews, webPreviews]);
  const availableWebPreviewIds = useMemo(
    () => availableWebPreviews.map((preview) => preview.id),
    [availableWebPreviews],
  );
  const availableWebPreviewFrameIds = useMemo(
    () => availableWebPreviewIds.map((previewId) =>
      webPreviewFrameId(selectedWorkspace ?? "", previewId)),
    [availableWebPreviewIds, selectedWorkspace],
  );
  useRetainedWebPreviewFrames(availableWebPreviewFrameIds);
  const activeWorkspaceView: WorkspaceControlView | undefined = mode === "images" || mode === "network" || mode === "repositories" || mode === "settings"
    ? mode
    : undefined;
  const navigateToMode = (next: WorkbenchMode) => {
    if (!selectedWorkspace) return;
    if (next === "images" || next === "network" || next === "repositories" || next === "settings") {
      void navigate({
        to: `/workspaces/$workspace/${next}`,
        params: { workspace: selectedWorkspace },
      });
      return;
    }
    if (!selectedPodId) {
      void navigate({
        to: "/workspaces/$workspace",
        params: { workspace: selectedWorkspace },
      });
      return;
    }
    if (next === "agent") {
      if (selectedAgentId) {
        void navigate({
          to: "/workspaces/$workspace/pods/$pod/chats/$chat",
          params: {
            workspace: selectedWorkspace,
            pod: selectedPodId,
            chat: selectedAgentId,
          },
        });
      } else {
        void navigate({
          to: "/workspaces/$workspace/pods/$pod",
          params: { workspace: selectedWorkspace, pod: selectedPodId },
        });
      }
      return;
    }
    void navigate({
      to: `/workspaces/$workspace/pods/$pod/${next}`,
      params: { workspace: selectedWorkspace, pod: selectedPodId },
    });
  };

  useEffect(
    () => storeValue(LAYOUT_STORAGE_KEY, JSON.stringify(storedLayouts)),
    [storedLayouts],
  );

  useEffect(() => {
    setPublishedPreviewUrls((current) => {
      const entries = Object.entries(current).filter(([id]) => publishedWebPreviewIds.has(id));
      return entries.length === Object.keys(current).length ? current : Object.fromEntries(entries);
    });
  }, [publishedWebPreviewIds]);

  useEffect(() => {
    setActiveWebPreviewId((current) => current && availableWebPreviews.some((preview) => preview.id === current)
      ? current
      : availableWebPreviews[0]?.id);
  }, [availableWebPreviews]);

  useEffect(() => {
    if (!layout.terminalOpen) {
      handledTerminalPanelTarget.current = undefined;
      return;
    }
    if (
      !terminalTabsReady
      || handledTerminalPanelTarget.current === terminalPanelTarget
    ) return;
    handledTerminalPanelTarget.current = terminalPanelTarget;
    const runningTerminal = terminalTabs.find((terminal) => terminal.status === "running");
    if (!runningTerminal) {
      onNewTerminal();
    } else if (runningTerminal.id !== activeTerminalId) {
      onSelectTerminal(runningTerminal.id);
    }
  }, [
    activeTerminalId,
    layout.terminalOpen,
    onNewTerminal,
    onSelectTerminal,
    terminalPanelTarget,
    terminalTabs,
    terminalTabsReady,
  ]);

  useEffect(() => {
    const availableIds = new Set(availableWebPreviewIds);
    setWebPreviewRevisions((current) => {
      const entries = Object.entries(current).filter(([id]) => availableIds.has(id));
      return entries.length === Object.keys(current).length ? current : Object.fromEntries(entries);
    });
  }, [availableWebPreviewIds]);

  useEffect(() => {
    const togglePanel = (event: KeyboardEvent) => {
      if (event.defaultPrevented || event.repeat || event.isComposing) return;
      if (!event.metaKey && !event.ctrlKey) return;
      const key = event.key.toLowerCase();
      let panel: "sidebarOpen" | "previewOpen" | "terminalOpen" | undefined;
      if (key === "b" && event.altKey && !event.shiftKey) panel = "previewOpen";
      else if (key === "b" && !event.altKey && !event.shiftKey) panel = "sidebarOpen";
      else if (key === "j" && !event.altKey && !event.shiftKey) panel = "terminalOpen";
      if (!panel) return;
      event.preventDefault();
      event.stopPropagation();
      setStoredLayouts((current) =>
        updateStoredLayout(current, layoutTarget, (layout) => ({
          [panel]: !layout[panel],
        }))
      );
    };
    window.addEventListener("keydown", togglePanel, { capture: true });
    return () => window.removeEventListener("keydown", togglePanel, { capture: true });
  }, [layoutTarget]);

  const updateLayout = (change: Partial<LayoutState>) => {
    setStoredLayouts((current) => updateStoredLayout(current, layoutTarget, () => change));
  };
  const selectTerminal = (terminalId: string) => {
    onSelectTerminal(terminalId);
    updateLayout({ terminalOpen: true });
  };
  const addTerminal = () => {
    handledTerminalPanelTarget.current = terminalPanelTarget;
    onNewTerminal();
    updateLayout({ terminalOpen: true });
  };
  const activeWebPreview = availableWebPreviews.find((preview) => preview.id === activeWebPreviewId);
  const selectWebPreview = (previewId: string) => {
    setActiveWebPreviewId(previewId);
    updateLayout({ previewOpen: true });
  };
  const closeWebPreview = (previewId: string) => {
    const index = availableWebPreviews.findIndex((preview) => preview.id === previewId);
    const nextManualPreviews = webPreviews.filter((preview) => preview.id !== previewId);
    const next = [
      ...availableWebPreviews.filter((preview) => publishedWebPreviewIds.has(preview.id)),
      ...nextManualPreviews,
    ];
    if (previewId === activeWebPreviewId) {
      setActiveWebPreviewId(next[Math.min(Math.max(index, 0), next.length - 1)]?.id);
    }
    setWebPreviews(nextManualPreviews);
  };
  const addWebPreview = () => {
    const number = nextWebPreviewNumber.current++;
    const preview = { id: `preview-${number}`, title: "New preview", url: "" };
    setWebPreviews((current) => [...current, preview]);
    setActiveWebPreviewId(preview.id);
    updateLayout({ previewOpen: true });
  };
  const reloadWebPreview = (previewId: string) => {
    setWebPreviewRevisions((current) => ({
      ...current,
      [previewId]: (current[previewId] ?? 0) + 1,
    }));
  };
  const navigateWebPreview = (previewId: string, address: string) => {
    const url = normalizePreviewUrl(address);
    if (!url) return;
    if (publishedWebPreviewIds.has(previewId)) {
      setPublishedPreviewUrls((current) => ({ ...current, [previewId]: url }));
    } else {
      setWebPreviews((current) => current.map((preview) => preview.id === previewId
        ? { ...preview, title: previewTitleForUrl(url), url }
        : preview));
    }
    reloadWebPreview(previewId);
  };

  const globalCommands: GlobalCommandDefinition[] = [
    ...(Object.entries(MODE_PRESENTATION) as Array<[
      WorkbenchMode,
      (typeof MODE_PRESENTATION)[WorkbenchMode],
    ]>).map(([commandMode, commandPresentation], index) => ({
      id: `workbench.view.${commandMode}`,
      label: `Open ${commandPresentation.label}`,
      description: commandMode === "agent" ? "Chats and agents" : `${commandPresentation.label} view`,
      group: "Navigation",
      keywords: ["view", "tab", commandMode],
      icon: commandPresentation.icon,
      order: 10 + index,
      available: Boolean(selectedWorkspace)
        && !workspaceScreen
        && (!POD_WORKBENCH_MODES.has(commandMode) || Boolean(selectedPodId)),
      disabled: mode === commandMode,
      perform: () => navigateToMode(commandMode),
    })),
    ...workspaces.map((candidate, index) => ({
      id: `workspace.open.${candidate.name}`,
      label: `Open Workspace: ${candidate.name}`,
      description: workspaceStateLabel(candidate),
      group: "Workspaces",
      keywords: ["switch", "workspace", candidate.name],
      icon: Monitor,
      order: 100 + index,
      disabled: candidate.name === selectedWorkspace,
      perform: () => onSelectWorkspace(candidate.name),
    })),
    ...pods.map((pod, index) => ({
      id: `pod.open.${pod.id}`,
      label: `Open Pod: ${pod.title}`,
      description: pod.status.status,
      group: "Pods",
      keywords: ["switch", "pod", pod.title, pod.id],
      icon: Box,
      order: 200 + index,
      available: !workspaceScreen,
      disabled: pod.id === selectedPodId && !activeWorkspaceView,
      perform: () => onSelectPod(pod.id),
    })),
    {
      id: "agent.new",
      label: "New Agent",
      description: "Start a chat in the current pod",
      group: "Create",
      keywords: ["chat", "conversation"],
      icon: Bot,
      order: 300,
      available: Boolean(selectedPodId) && !workspaceScreen,
      perform: onNewAgent,
    },
    {
      id: "code.new",
      label: "New Code Session",
      description: "Choose a workspace folder",
      group: "Create",
      keywords: ["editor", "repository", "folder", "working directory"],
      icon: Code2,
      order: 301,
      available: Boolean(selectedPodId) && !workspaceScreen,
      perform: () => {
        navigateToMode("code");
        onNewCodeSession();
      },
    },
    {
      id: "terminal.new",
      label: "New Terminal",
      description: "Open a shell in the current pod",
      group: "Create",
      keywords: ["shell", "process"],
      icon: TerminalSquare,
      order: 302,
      available: Boolean(selectedPodId) && !workspaceScreen,
      perform: addTerminal,
    },
    {
      id: "layout.sidebar.toggle",
      label: layout.sidebarOpen ? "Hide Workspace Sidebar" : "Show Workspace Sidebar",
      group: "Layout",
      keywords: ["toggle", "panel", "navigation"],
      shortcut: SIDEBAR_SHORTCUT,
      order: 400,
      perform: () => updateLayout({ sidebarOpen: !layout.sidebarOpen }),
    },
    {
      id: "layout.preview.toggle",
      label: layout.previewOpen ? "Hide Web Preview" : "Show Web Preview",
      group: "Layout",
      keywords: ["toggle", "panel", "browser"],
      shortcut: PREVIEW_SHORTCUT,
      order: 401,
      available: Boolean(selectedPodId) && !activeWorkspaceView && !workspaceScreen,
      perform: () => updateLayout({ previewOpen: !layout.previewOpen }),
    },
    {
      id: "layout.terminal.toggle",
      label: layout.terminalOpen ? "Hide Terminals" : "Show Terminals",
      group: "Layout",
      keywords: ["toggle", "panel", "shell"],
      shortcut: TERMINAL_SHORTCUT,
      order: 402,
      available: Boolean(selectedPodId) && !activeWorkspaceView && !workspaceScreen,
      perform: () => updateLayout({ terminalOpen: !layout.terminalOpen }),
    },
  ];
  useGlobalCommands(globalCommands);

  return (
    <div className="tascarrel-shell" data-sidebar-open={layout.sidebarOpen || undefined}>
      <ConnectionOverlay connection={workspaceConnection} attempt={workspaceConnectionAttempt} />
      <div className="workspace-sidebar-dock">
        {layout.sidebarOpen ? (
          <WorkspaceSidebar
            workspaces={workspaces}
            selectedWorkspace={selectedWorkspace}
            usbEnabled={usbEnabled}
            pods={pods}
            podChangeSummaries={podChangeSummaries}
            podChangeSummariesVerified={podChangeSummariesVerified}
            podListEmptyMessage={podListEmptyMessage}
            showPodCount={showPodCount}
            selectedPodId={selectedPodId}
            busyPodIds={busyPodIds}
            attentionPodIds={attentionPodIds}
            agentNeedsInput={agentNeedsInput}
            repositoryApprovalCount={repositoryApprovalCount}
            activeWorkspaceView={activeWorkspaceView}
            shortcut={SIDEBAR_SHORTCUT}
            onSelectWorkspace={onSelectWorkspace}
            onCreateWorkspace={onCreateWorkspace}
            onStartWorkspace={onStartWorkspace}
            onStopWorkspace={onStopWorkspace}
            onSelectPod={(podId) => {
              onSelectPod(podId);
            }}
            canCreatePod={canCreatePod}
            podCreationActive={podCreationActive}
            onCreatePod={onCreatePod}
            onSelectWorkspaceView={navigateToMode}
            onStartPod={onStartPod}
            onStopPod={onStopPod}
            onDestroyPod={onDestroyPod}
            onCollapse={() => updateLayout({ sidebarOpen: false })}
          />
        ) : (
          <ShellPanelToggle
            className="workspace-sidebar-expand"
            side="left"
            expanded={false}
            label="workspace sidebar"
            shortcut={SIDEBAR_SHORTCUT}
            shortcutKeys="Control+B Meta+B"
            aria-controls="workspace-sidebar"
            onClick={() => updateLayout({ sidebarOpen: true })}
          />
        )}
      </div>
      <main className="workbench-deck">
        {workspaceScreen ? (
          <div className="min-h-0 flex-1 overflow-auto">{workspaceScreen}</div>
        ) : (
          <div className="workbench-layout">
          <div className="workbench-upper">
            <section className="workbench-main" aria-label={`${presentation.label} view`}>
              {!activeWorkspaceView ? (
                <header className="shell-tab-bar workbench-tabs">
                <WorkbenchModeNav
                  value={mode}
                  onValueChange={navigateToMode}
                />
                {mode === "agent" || mode === "code" ? (
                  <ShellTabStrip
                    label={`${presentation.label} tabs`}
                    action={mode === "agent"
                      ? <ShellTabAction label="New agent" onClick={onNewAgent} />
                      : <ShellTabAction label="New code session" onClick={onNewCodeSession} />}
                  >
                    {mode === "agent" ? (
                      <>
                        {agentTabs.map((tab) => (
                          <ShellTab
                            active={!creatingAgent && selectedAgentId === tab.id}
                            attention={tab.attention}
                            aria-label={tab.attention ? `${tab.title} · Needs attention` : tab.title}
                            closeLabel={`Archive ${tab.title || "untitled agent"}`}
                            title={tab.attention ? `${tab.title} · Needs attention` : tab.title}
                            key={tab.id}
                            onClose={() => onArchiveAgent(tab.id)}
                            onClick={() => onSelectAgent(tab.id)}
                          >
                            <AgentTabStatus status={tab.status} />
                            <span className="shell-tab-title">{tab.title || "Untitled agent"}</span>
                          </ShellTab>
                        ))}
                        {creatingAgent ? (
                          <ShellTab active>
                            <span className="shell-tab-title">New agent</span>
                          </ShellTab>
                        ) : null}
                      </>
                    ) : (
                      codeTabs.map((tab) => (
                        <ShellTab
                          active={selectedCodeFolder === tab.folder}
                          failure={tab.status === "failed"}
                          closeLabel={tab.closeable ? `Close ${tab.title}` : undefined}
                          title={tab.folder}
                          key={tab.folder}
                          onClose={tab.closeable ? () => onCloseCodeSession(tab.folder) : undefined}
                          onClick={() => onSelectCodeSession(tab.folder)}
                        >
                          <CodeTabStatus status={tab.status} />
                          <span className="shell-tab-title">{tab.title}</span>
                        </ShellTab>
                      ))
                    )}
                  </ShellTabStrip>
                ) : null}
                </header>
              ) : null}

              <div className="workbench-stage">
                <div className="workbench-agent-host" hidden={mode !== "agent"}>
                  {agentView}
                </div>
                {mode === "code" ? codeView : null}
                {mode === "changes" ? changesView : null}
                {mode === "files" ? filesView : null}
                {mode === "pod" ? (
                  <PodOverview
                    workspace={selectedWorkspace}
                    pod={pods.find((candidate) => candidate.id === selectedPodId)}
                    processView={podProcessView}
                  />
                ) : null}
                {mode === "images" ? imagesView : null}
                {mode === "network" ? networkView : null}
                {mode === "repositories" ? repositoriesView : null}
                {mode === "settings" ? settingsView : null}
              </div>
            </section>

            {!activeWorkspaceView ? (
              <ResizableDock
              side="right"
              label="web preview"
              open={layout.previewOpen}
              size={layout.previewSize}
              minSize={300}
              maxSize={MAX_PREVIEW_SIZE}
              defaultSize={DEFAULT_LAYOUT.previewSize}
              shortcut={PREVIEW_SHORTCUT}
              shortcutKeys="Control+Alt+B Meta+Alt+B"
              onOpenChange={(previewOpen) => updateLayout({ previewOpen })}
              onSizeChange={(previewSize) => updateLayout({ previewSize })}
              header={(
                <>
                  <ShellModeNav value="web" options={WEB_PANEL_MODES} label="Web panel mode" />
                  <ShellTabStrip
                    label="Web previews"
                    action={<ShellTabAction label="New web preview" onClick={addWebPreview} />}
                  >
                    {availableWebPreviews.map((preview) => (
                      <ShellTab
                        active={activeWebPreviewId === preview.id}
                        closeLabel={publishedWebPreviewIds.has(preview.id)
                          ? undefined
                          : `Close ${preview.title}`}
                        title={preview.url || preview.title}
                        key={preview.id}
                        onClose={publishedWebPreviewIds.has(preview.id)
                          ? undefined
                          : () => closeWebPreview(preview.id)}
                        onClick={() => selectWebPreview(preview.id)}
                      >
                        <span className="shell-tab-title">{preview.title}</span>
                      </ShellTab>
                    ))}
                  </ShellTabStrip>
                </>
              )}
            >
              {activeWebPreview ? (
                <WebPreviewView
                  frameId={webPreviewFrameId(selectedWorkspace ?? "", activeWebPreview.id)}
                  preview={activeWebPreview}
                  revision={webPreviewRevisions[activeWebPreview.id] ?? 0}
                  onNavigate={(address) => navigateWebPreview(activeWebPreview.id, address)}
                  onReload={() => reloadWebPreview(activeWebPreview.id)}
                />
              ) : (
                <ShellPlaceholder
                  icon={Monitor}
                  title="No web previews"
                  detail="Create a preview to inspect a published pod port or external site."
                />
              )}
              </ResizableDock>
            ) : null}
          </div>

          {!activeWorkspaceView ? (
            <ResizableDock
            side="bottom"
            label="terminals"
            open={layout.terminalOpen}
            size={layout.terminalSize}
            minSize={160}
            maxSize={580}
            defaultSize={DEFAULT_LAYOUT.terminalSize}
            shortcut={TERMINAL_SHORTCUT}
            shortcutKeys="Control+J Meta+J"
            onOpenChange={(terminalOpen) => updateLayout({ terminalOpen })}
            onSizeChange={(terminalSize) => updateLayout({ terminalSize })}
            header={(
              <>
                <ShellModeNav value="terminal" options={TERMINAL_PANEL_MODES} label="Terminal panel mode" />
                <ShellTabStrip
                  label="Terminals"
                  action={<ShellTabAction label="New terminal" onClick={addTerminal} />}
                >
                  {terminalTabs.map((terminal) => (
                    <ShellTab
                      active={activeTerminalId === terminal.id}
                      failure={terminal.status === "failed"}
                      closeLabel={`Close ${terminal.title || "terminal"}`}
                      title={terminal.title}
                      key={terminal.id}
                      onClose={() => onCloseTerminal(terminal.id)}
                      onClick={() => selectTerminal(terminal.id)}
                    >
                      <span className="shell-tab-title">{terminal.title || "Terminal"}</span>
                    </ShellTab>
                  ))}
                </ShellTabStrip>
              </>
            )}
          >
            {terminalTabs.length > 0 ? terminalView : (
              <ShellPlaceholder
                icon={TerminalSquare}
                title="No terminals"
                detail="Create a terminal to open a shell in the local workbench."
              />
            )}
            </ResizableDock>
          ) : null}
          </div>
        )}
      </main>
    </div>
  );
}

function AgentTabStatus({ status }: { status?: AgentWorkbenchTab["status"] }) {
  if (status !== "working") return null;
  return (
    <LoaderCircle
      className="workbench-tab-spinner animate-spin"
      size={11}
      aria-label="Agent working"
    />
  );
}

function CodeTabStatus({ status }: { status: CodeWorkbenchTab["status"] }) {
  if (status !== "starting") return null;
  return (
    <LoaderCircle
      className="workbench-tab-spinner animate-spin"
      size={11}
      aria-label="Code session starting"
    />
  );
}

function workspaceStateLabel(workspace: workspaces.Workspace): string {
  switch (workspace.state.status) {
    case "Running": return "Running";
    case "Starting": return "Starting";
    case "Stopping": return "Stopping";
    case "Stopped": return "Stopped";
    case "Failed": return "Failed";
    case "Destroying": return "Destroying";
  }
}

function loadStoredWorkbenchLayouts(): StoredWorkbenchLayouts {
  const stored = loadValue(LAYOUT_STORAGE_KEY);
  if (stored) {
    try {
      const value = JSON.parse(stored) as Partial<StoredWorkbenchLayouts>;
      const defaultLayout = normalizeWorkbenchLayout(value.defaultLayout);
      const layoutsByTarget = Object.fromEntries(
        Object.entries(value.layoutsByTarget ?? {}).map(([layoutTarget, layout]) => [
          layoutTarget,
          normalizeWorkbenchLayout(layout, defaultLayout),
        ]),
      );
      return { defaultLayout, layoutsByTarget };
    } catch {
      return { defaultLayout: loadLegacyWorkbenchLayout(), layoutsByTarget: {} };
    }
  }
  return { defaultLayout: loadLegacyWorkbenchLayout(), layoutsByTarget: {} };
}

function updateStoredLayout(
  storedLayouts: StoredWorkbenchLayouts,
  layoutTarget: string | undefined,
  layoutChange: (layout: LayoutState) => Partial<LayoutState>,
): StoredWorkbenchLayouts {
  const currentLayout = layoutTarget
    ? storedLayouts.layoutsByTarget[layoutTarget] ?? storedLayouts.defaultLayout
    : storedLayouts.defaultLayout;
  const updatedLayout = { ...currentLayout, ...layoutChange(currentLayout) };
  return layoutTarget
    ? {
        ...storedLayouts,
        layoutsByTarget: {
          ...storedLayouts.layoutsByTarget,
          [layoutTarget]: updatedLayout,
        },
      }
    : { ...storedLayouts, defaultLayout: updatedLayout };
}

function loadLegacyWorkbenchLayout(): LayoutState {
  const stored = loadValue(LEGACY_LAYOUT_STORAGE_KEY);
  if (!stored) return DEFAULT_LAYOUT;
  try {
    return normalizeWorkbenchLayout(JSON.parse(stored));
  } catch {
    return DEFAULT_LAYOUT;
  }
}

function normalizeWorkbenchLayout(value: unknown, fallback = DEFAULT_LAYOUT): LayoutState {
  const layout = value && typeof value === "object"
    ? value as Partial<LayoutState>
    : {};
  return {
    sidebarOpen: typeof layout.sidebarOpen === "boolean"
      ? layout.sidebarOpen
      : fallback.sidebarOpen,
    terminalOpen: typeof layout.terminalOpen === "boolean"
      ? layout.terminalOpen
      : fallback.terminalOpen,
    terminalSize: clampNumber(layout.terminalSize, 160, 580, fallback.terminalSize),
    previewOpen: typeof layout.previewOpen === "boolean"
      ? layout.previewOpen
      : fallback.previewOpen,
    previewSize: clampNumber(layout.previewSize, 300, MAX_PREVIEW_SIZE, fallback.previewSize),
  };
}

function clampNumber(value: unknown, minimum: number, maximum: number, fallback: number) {
  return typeof value === "number" && Number.isFinite(value)
    ? Math.min(maximum, Math.max(minimum, value))
    : fallback;
}

function loadValue(key: string): string | undefined {
  try {
    return window.localStorage.getItem(key) ?? undefined;
  } catch {
    return undefined;
  }
}

function storeValue(key: string, value: string) {
  try {
    window.localStorage.setItem(key, value);
  } catch {
    // Storage is a progressive enhancement for the temporary shell.
  }
}
