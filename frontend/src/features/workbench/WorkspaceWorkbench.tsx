import { X } from "lucide-react";
import { useNavigate } from "@tanstack/react-router";
import {
  lazy,
  Suspense,
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
  type ReactNode,
} from "react";

import { chatAttachmentUrl, uploadChatAttachment } from "../../api/attachments.ts";
import { guestApi } from "../../api/client.ts";
import type {
  changes,
  chats,
  code,
  config,
  network,
  pods,
  processes,
  repositories,
  workspaces,
} from "../../api/generated/index.ts";
import { useMobileLayout } from "../../app/layout.ts";
import type { WorkbenchRoute, WorkspaceScreenName } from "../../app/router.tsx";
import { Button } from "../../components/ui/Button.tsx";
import { ConfirmDialog } from "../../components/ui/ConfirmDialog.tsx";
import {
  ChatScreen,
  ChatStartScreen,
  type PromptSubmission,
  type StartChatSubmission,
} from "../chat/index.ts";
import { removeChatComposerDraft } from "../chat/model/drafts.ts";
import { harnessKindKey } from "../chat/model/format.ts";
import { chatModelPreferences } from "../chat/model/modelPreferences.ts";
import { useChat, useChatHarnesses, useChatList } from "../chat/state.ts";
import { codeFolderLabel, DEFAULT_CODE_FOLDER } from "../code/folders.ts";
import { useCodeSessions } from "../code/state.ts";
import { useHostOperations } from "../hostOperations/state.ts";
import { MobileChangesView } from "../changes/MobileChangesView.tsx";
import {
  summarizePodChanges,
  type PodChangeSummary,
} from "../changes/podChangeSummary.ts";
import { useRepositoryStatuses } from "../changes/state.ts";
import { useHttpRoutes } from "../network/state.ts";
import { isPodStarting, PodStartupScreen } from "../pods/PodStartupScreen.tsx";
import { usePods } from "../pods/state.ts";
import { useProcesses } from "../processes/state.ts";
import { MobileRepositoryApprovals } from "../repositories/MobileRepositoryApprovals.tsx";
import { useRepositories, useRepositoryApprovals } from "../repositories/state.ts";
import { useWorkspaceConfig } from "../workspaces/runtimeState.ts";
import {
  WorkspaceLifecycleScreen,
  workspaceScreenForState,
} from "../workspaces/WorkspaceLifecycleScreens.tsx";
import type {
  AgentWorkbenchTab,
  CodeWorkbenchTab,
  TerminalWorkbenchTab,
} from "./shell/WorkbenchShell.tsx";
import type { WebPreview } from "./shell/WebPreview.tsx";
import { MobileWorkbenchShell } from "./mobile/MobileWorkbenchShell.tsx";
import { mobileChatSummary } from "./mobile/MobilePodList.tsx";
import { MobileWorkspaceStatus } from "./mobile/MobileWorkspaceHome.tsx";

type WorkspaceConnection = "idle" | "connecting" | "live" | "reconnecting";
const EMPTY_POD_CHANGE_SUMMARIES: ReadonlyMap<pods.PodId, PodChangeSummary> = new Map();
const CodeDirectoryPalette = lazy(() =>
  import("../code/CodeDirectoryPalette.tsx").then((module) => ({
    default: module.CodeDirectoryPalette,
  }))
);
const CodeDirectoryPicker = lazy(() =>
  import("../code/CodeDirectoryPalette.tsx").then((module) => ({
    default: module.CodeDirectoryPicker,
  }))
);
const CodeView = lazy(() =>
  import("../code/CodeView.tsx").then((module) => ({ default: module.CodeView }))
);
const DesktopChangesView = lazy(() =>
  import("../changes/ChangesView.tsx").then((module) => ({ default: module.ChangesView }))
);
const FilesView = lazy(() =>
  import("../files/FilesView.tsx").then((module) => ({ default: module.FilesView }))
);
const ImagesView = lazy(() =>
  import("../images/ImagesView.tsx").then((module) => ({ default: module.ImagesView }))
);
const NetworkView = lazy(() =>
  import("../network/NetworkView.tsx").then((module) => ({ default: module.NetworkView }))
);
const HostOperationsView = lazy(() =>
  import("../hostOperations/HostOperationsView.tsx").then((module) => ({
    default: module.HostOperationsView,
  }))
);
const ProcessManager = lazy(() =>
  import("../processes/ProcessManager.tsx").then((module) => ({
    default: module.ProcessManager,
  }))
);
const ProcessTerminal = lazy(() =>
  import("../processes/ProcessTerminal.tsx").then((module) => ({
    default: module.ProcessTerminal,
  }))
);
const RepositoryApprovalOverlay = lazy(() =>
  import("../repositories/RepositoryApprovalOverlay.tsx").then((module) => ({
    default: module.RepositoryApprovalOverlay,
  }))
);
const RepositoriesView = lazy(() =>
  import("../repositories/RepositoriesView.tsx").then((module) => ({
    default: module.RepositoriesView,
  }))
);
const WorkspaceSettings = lazy(() =>
  import("../settings/WorkspaceSettings.tsx").then((module) => ({
    default: module.WorkspaceSettings,
  }))
);
const WorkbenchShell = lazy(() =>
  import("./shell/WorkbenchShell.tsx").then((module) => ({
    default: module.WorkbenchShell,
  }))
);

type CodeSelection = {
  podId: pods.PodId;
  folder: string;
};

export function WorkspaceWorkbench({
  allWorkspaces,
  workspace,
  route,
  workspaceConnection,
  workspaceConnectionAttempt,
  onSelectWorkspace,
  onCreateWorkspace,
  onStartWorkspace,
  onStopWorkspace,
}: {
  allWorkspaces: readonly workspaces.Workspace[];
  workspace: workspaces.WorkspaceName;
  route: WorkbenchRoute;
  workspaceConnection: WorkspaceConnection;
  workspaceConnectionAttempt: number;
  onSelectWorkspace: (workspace: workspaces.WorkspaceName) => void;
  onCreateWorkspace: () => void;
  onStartWorkspace: (workspace: workspaces.WorkspaceName) => Promise<void>;
  onStopWorkspace: (workspace: workspaces.WorkspaceName) => Promise<void>;
}) {
  const navigate = useNavigate();
  const mobileLayout = useMobileLayout();
  const podState = usePods(workspace);
  const chatListState = useChatList(workspace);
  const harnessState = useChatHarnesses(workspace);
  const configState = useWorkspaceConfig(workspace);
  const processState = useProcesses(workspace);
  const httpRouteState = useHttpRoutes(workspace);
  const codeSessionState = useCodeSessions(workspace);
  const repositoryState = useRepositories(workspace);
  const repositoryApprovalState = useRepositoryApprovals(workspace);
  const hostOperationState = useHostOperations(workspace);
  const repositoryStatusState = useRepositoryStatuses(workspace);
  const pods = podState.value?.pods ?? [];
  const currentWorkspace = allWorkspaces.find((candidate) => candidate.name === workspace);
  const workspacePanelOpen = route.creatingPod === true
    || route.view === "images"
    || route.view === "network"
    || route.view === "repositories"
    || route.view === "operations"
    || route.view === "settings";
  const routedPod = pods.find((pod) => pod.id === route.pod);
  const selectedPod = workspacePanelOpen
    ? undefined
    : routedPod ?? (mobileLayout ? undefined : pods[0]);
  const selectedPodId = selectedPod?.id;
  const publishedWebPreviews = useMemo(
    () => httpRoutePreviews(httpRouteState.value?.httpRoutes ?? [], selectedPodId),
    [httpRouteState.value?.httpRoutes, selectedPodId],
  );
  const retainedPublishedWebPreviewIds = useMemo(
    () => (httpRouteState.value?.httpRoutes ?? [])
      .filter((route) => !route.internal)
      .map(httpRoutePreviewId),
    [httpRouteState.value?.httpRoutes],
  );
  const podChangeSummaries = useMemo(
    () => summarizePodChanges(repositoryStatusState.value?.repositories ?? []),
    [repositoryStatusState.value?.repositories],
  );
  const [startingChat, setStartingChat] = useState(false);
  const [error, setError] = useState<string>();
  const [archiveTarget, setArchiveTarget] = useState<chats.ChatSummary>();
  const [archiving, setArchiving] = useState(false);
  const [codeDirectoryPaletteOpen, setCodeDirectoryPaletteOpen] = useState(false);
  const [codeSelection, setCodeSelection] = useState<CodeSelection>();
  const [activeTerminalId, setActiveTerminalId] = useState<processes.ProcessId>();
  const [dismissedTerminalIds, setDismissedTerminalIds] = useState<ReadonlySet<string>>(
    () => new Set(),
  );
  const pendingTerminalRemoval = useRef(new Set<string>());
  const workspaceChats = chatListState.value?.chats ?? [];
  const busyPodIds = useMemo(
    () => new Set(
      workspaceChats
        .filter((chat) => chat.agentStatus === "Working")
        .map((chat) => chat.podId),
    ),
    [workspaceChats],
  );
  const attentionPodIds = useMemo(
    () => new Set(
      workspaceChats
        .filter((chat) => chat.attentionRequired)
        .map((chat) => chat.podId),
    ),
    [workspaceChats],
  );
  const podChats = useMemo(
    () => selectedPodId
      ? workspaceChats.filter((chat) => chat.podId === selectedPodId)
      : [],
    [selectedPodId, workspaceChats],
  );
  const selectedSummary = podChats.find((chat) => chat.chatId === route.chat);
  const podTerminals = useMemo(
    () => selectedPodId
      ? (processState.value?.processes ?? []).filter((process) =>
          process.podId === selectedPodId
          && process.terminal !== undefined
          && !dismissedTerminalIds.has(process.id)
        )
      : [],
    [dismissedTerminalIds, processState.value?.processes, selectedPodId],
  );
  const activeTerminal = podTerminals.find((process) => process.id === activeTerminalId);
  const selectedCodeFolder = selectedPodId && codeSelection?.podId === selectedPodId
    ? codeSelection.folder
    : undefined;
  const podCodeSessions = selectedPodId
    ? (codeSessionState.value?.codeSessions ?? []).filter((session) => session.podId === selectedPodId)
    : [];
  const repositories = repositoryState.value?.repositories ?? [];
  const codeTabs = codeSessionTabs(
    selectedPodId,
    selectedCodeFolder,
    podCodeSessions,
    repositories,
  );

  useEffect(() => {
    if (mobileLayout || !selectedPodId || route.pod === selectedPodId) return;
    void navigate({
      to: "/workspaces/$workspace/pods/$pod",
      params: { workspace, pod: selectedPodId },
      replace: true,
    });
  }, [mobileLayout, navigate, route.pod, selectedPodId, workspace]);

  useEffect(() => {
    if (activeTerminal) return;
    setActiveTerminalId(podTerminals.at(-1)?.id);
  }, [activeTerminal, podTerminals]);

  useEffect(() => {
    for (const processId of pendingTerminalRemoval.current) {
      const process = processState.value?.processes.find((candidate) => candidate.id === processId);
      if (!process) {
        pendingTerminalRemoval.current.delete(processId);
        continue;
      }
      if (process.status.status !== "Exited" && process.status.status !== "Failed") continue;
      pendingTerminalRemoval.current.delete(processId);
      void guestApi(workspace)
        .execute("processes_Remove", { processId: process.id })
        .catch((cause) => {
          setDismissedTerminalIds((current) => withoutValue(current, processId));
          reportError(cause);
        });
    }
  }, [processState.value?.processes, workspace]);

  useEffect(() => {
    if (
      mobileLayout
      || route.view !== "agent"
      || !selectedPodId
      || startingChat
      || !chatListState.ready
      || selectedSummary
    ) return;
    const first = podChats[0];
    if (first) {
      void navigate({
        to: "/workspaces/$workspace/pods/$pod/chats/$chat",
        params: { workspace, pod: selectedPodId, chat: first.chatId },
        replace: true,
      });
    }
    else setStartingChat(true);
  }, [
    chatListState.ready,
    mobileLayout,
    navigate,
    podChats,
    route.view,
    selectedPodId,
    selectedSummary,
    startingChat,
    workspace,
  ]);

  useEffect(() => {
    if (
      route.view !== "agent"
      || startingChat
      || !selectedSummary?.attentionRequired
    ) return;
    void guestApi(workspace)
      .execute("chats_AcknowledgeAttention", { chatId: selectedSummary.chatId })
      .catch((cause) => setError(cause instanceof Error ? cause.message : String(cause)));
  }, [route.view, selectedSummary?.attentionRequired, selectedSummary?.chatId, startingChat, workspace]);

  const selectPod = (podId: pods.PodId) => {
    setStartingChat(false);
    void navigate({
      to: "/workspaces/$workspace/pods/$pod",
      params: { workspace, pod: podId },
      search: {},
    });
  };
  const selectChat = (chatId: chats.ChatId) => {
    setStartingChat(false);
    if (!selectedPodId) return;
    void navigate({
      to: "/workspaces/$workspace/pods/$pod/chats/$chat",
      params: { workspace, pod: selectedPodId, chat: chatId },
      search: {},
    });
  };
  const selectWorkspaceChat = (podId: pods.PodId, chatId: chats.ChatId) => {
    setStartingChat(false);
    void navigate({
      to: "/workspaces/$workspace/pods/$pod/chats/$chat",
      params: { workspace, pod: podId, chat: chatId },
      search: {},
    });
  };
  const newChat = () => {
    if (!selectedPodId) return;
    setStartingChat(true);
    void navigate({
      to: "/workspaces/$workspace/pods/$pod",
      params: { workspace, pod: selectedPodId },
      search: {},
    });
  };
  const togglePodCreation = () => {
    setStartingChat(false);
    if (route.creatingPod) {
      const fallbackPod = pods[0];
      void navigate(fallbackPod
        ? {
            to: "/workspaces/$workspace/pods/$pod",
            params: { workspace, pod: fallbackPod.id },
          }
        : {
            to: "/workspaces/$workspace",
            params: { workspace },
          });
      return;
    }
    void navigate({
      to: "/workspaces/$workspace/pods/new",
      params: { workspace },
    });
  };
  const reportError = (cause: unknown) => {
    setError(cause instanceof Error ? cause.message : String(cause));
  };
  const uploadAttachment = useCallback(
    (file: File) => uploadChatAttachment(workspace, file),
    [workspace],
  );
  const attachmentUrl = useCallback(
    (attachmentId: chats.ChatAttachmentId) => chatAttachmentUrl(workspace, attachmentId),
    [workspace],
  );
  const harnesses = harnessState.value ?? [];
  const selectedHarness = selectedSummary
    ? harnesses.find((harness) => harnessKindKey(harness.kind) === harnessKindKey(selectedSummary.harness))
    : undefined;
  const settings = configState.value?.settings;
  const slashCommands = configState.value?.config?.chat?.commands;
  const agentTabs: AgentWorkbenchTab[] = podChats
    .toSorted((left, right) => String(left.createdAt).localeCompare(String(right.createdAt)))
    .map((summary) => ({
      id: summary.chatId,
      title: summary.title || "Untitled agent",
      status: agentTabStatus(summary),
      attention: summary.attentionRequired,
    }));
  const mobileChats = workspaceChats
    .toSorted((left, right) => String(right.updatedAt).localeCompare(String(left.updatedAt)))
    .map(mobileChatSummary);
  const visibleError = error
    ?? podState.error?.message
    ?? chatListState.error?.message
    ?? harnessState.error?.message
    ?? (mobileLayout ? undefined : processState.error?.message)
    ?? repositoryApprovalState.error?.message
    ?? repositoryStatusState.error?.message
    ?? configState.error?.message
    ?? configState.value?.lastConfigError?.message
    ?? configState.value?.lastSettingsError?.message;

  const archiveChat = async () => {
    if (!archiveTarget || archiving) return;
    setArchiving(true);
    try {
      await guestApi(workspace).execute("chats_Archive", { chatId: archiveTarget.chatId });
      removeChatComposerDraft(`chat:${archiveTarget.chatId}`);
      if (archiveTarget.chatId === selectedSummary?.chatId) {
        const next = podChats.find((candidate) => candidate.chatId !== archiveTarget.chatId);
        setStartingChat(!next);
        if (selectedPodId) {
          if (next) {
            void navigate({
              to: "/workspaces/$workspace/pods/$pod/chats/$chat",
              params: { workspace, pod: selectedPodId, chat: next.chatId },
            });
          } else {
            void navigate({
              to: "/workspaces/$workspace/pods/$pod",
              params: { workspace, pod: selectedPodId },
            });
          }
        }
      }
      setArchiveTarget(undefined);
    } catch (cause) {
      reportError(cause);
    } finally {
      setArchiving(false);
    }
  };

  const terminalTabs: TerminalWorkbenchTab[] = podTerminals.map((process) => ({
    id: process.id,
    title: process.title || "Terminal",
    status: terminalStatus(process),
  }));
  const newTerminal = () => {
    if (!selectedPodId) return;
    void guestApi(workspace)
      .execute("processes_SpawnTerminal", {
        podId: selectedPodId,
        title: `Terminal ${podTerminals.length + 1}`,
        terminal: DEFAULT_TERMINAL_SIZE,
      })
      .then((output) => setActiveTerminalId(output.processId))
      .catch(reportError);
  };
  const closeTerminal = (processId: string) => {
    const process = podTerminals.find((candidate) => candidate.id === processId);
    if (!process) return;
    setDismissedTerminalIds((current) => new Set(current).add(processId));
    if (process.status.status === "Exited" || process.status.status === "Failed") {
      void guestApi(workspace)
        .execute("processes_Remove", { processId: process.id })
        .catch((cause) => {
          setDismissedTerminalIds((current) => withoutValue(current, processId));
          reportError(cause);
        });
      return;
    }
    pendingTerminalRemoval.current.add(processId);
    void guestApi(workspace)
      .execute("processes_Kill", { processId: process.id, signal: { type: "Hangup" } })
      .catch((cause) => {
        pendingTerminalRemoval.current.delete(processId);
        setDismissedTerminalIds((current) => withoutValue(current, processId));
        reportError(cause);
      });
  };
  const closeCodeSession = (folder: string) => {
    const session = podCodeSessions.find((candidate) => candidate.folder === folder);
    if (!session || folder === DEFAULT_CODE_FOLDER) return;
    if (selectedCodeFolder === folder) setCodeSelection(undefined);
    void guestApi(workspace)
      .execute("code_DeleteSession", { codeSessionId: session.id })
      .catch(reportError);
  };

  const podCreationScreen = route.creatingPod ? (
    <div className="flex h-full flex-col overflow-hidden bg-canvas text-foreground">
      {visibleError ? (
        <div className="border-b border-red-500/20 bg-red-500/5 px-4 py-2.5">
          <InlineError message={visibleError} onClose={() => setError(undefined)} />
        </div>
      ) : null}
      <ChatStartScreen
        draftId={`workspace:${workspace}:new-pod`}
        harnesses={configState.ready || configState.error ? harnesses : []}
        settings={settings}
        slashCommands={slashCommands}
        creationTarget="pod"
        attachmentUploader={uploadAttachment}
        attachmentUrl={attachmentUrl}
        loading={!harnessState.ready || (!configState.ready && !configState.error)}
        onError={reportError}
        onCreateWithoutChat={async (title) => {
          const output = await createPod(workspace, title);
          await navigate({
            to: "/workspaces/$workspace/pods/$pod",
            params: { workspace, pod: output.podId },
          });
        }}
        onStart={async (submission) => {
          const output = await createPodChat(workspace, submission);
          await navigate({
            to: "/workspaces/$workspace/pods/$pod/chats/$chat",
            params: {
              workspace,
              pod: output.podId,
              chat: output.chatId,
            },
          });
        }}
      />
    </div>
  ) : undefined;
  const podStartupScreen = selectedPod && isPodStarting(selectedPod)
    ? <PodStartupScreen pod={selectedPod} workspace={workspace} />
    : undefined;

  const agentView = (
    <div className="flex h-full flex-col overflow-hidden bg-canvas text-foreground">
      {visibleError ? (
        <div className="border-b border-red-500/20 bg-red-500/5 px-4 py-2.5">
          <InlineError message={visibleError} onClose={() => setError(undefined)} />
        </div>
      ) : null}
      <div className="flex min-h-0 flex-1 overflow-hidden">
        {startingChat && selectedPodId ? (
          <ChatStartScreen
            draftId={`workspace:${workspace}:pod:${selectedPodId}`}
            harnesses={configState.ready || configState.error ? harnesses : []}
            settings={settings}
            slashCommands={slashCommands}
            attachmentUploader={uploadAttachment}
            attachmentUrl={attachmentUrl}
            loading={!harnessState.ready || (!configState.ready && !configState.error)}
            onError={reportError}
            onStart={async (submission) => {
              const output = await createChat(workspace, selectedPodId, submission);
              selectChat(output.chatId);
            }}
          />
        ) : selectedSummary ? (
          <SelectedChat
            workspace={workspace}
            summary={selectedSummary}
            harness={selectedHarness}
            modelPreferences={selectedHarness
              ? chatModelPreferences(settings, selectedHarness.kind)
              : undefined}
            usageSettings={settings?.usage}
            slashCommands={slashCommands}
            attachmentUploader={uploadAttachment}
            attachmentUrl={attachmentUrl}
            onError={reportError}
          />
        ) : (
          <div className="flex flex-1 items-center justify-center text-sm text-muted">
            {chatListState.ready ? "Select a pod to begin." : "Loading chats…"}
          </div>
        )}
      </div>
    </div>
  );

  return (
    <>
      <ConfirmDialog
        confirmLabel="Archive"
        description="This agent will be removed from the active tab list. It cannot currently be restored."
        destructive
        open={Boolean(archiveTarget)}
        pending={archiving}
        title="Archive Agent?"
        onOpenChange={(open) => {
          if (!open) setArchiveTarget(undefined);
        }}
        onConfirm={() => void archiveChat()}
      />
      {mobileLayout ? (
        <MobileWorkbenchShell
          workspaces={allWorkspaces}
          selectedWorkspace={workspace}
          pods={pods}
          podChangeSummaries={podChangeSummaries}
          podChangeSummariesVerified={repositoryStatusState.ready
            && !repositoryStatusState.error}
          selectedPodId={selectedPodId}
          selectedChatId={startingChat ? undefined : selectedSummary?.chatId}
          route={route}
          workspaceScreen={podCreationScreen ?? podStartupScreen}
          workspaceConnection={workspaceConnection === "live"
            ? podState.connection
            : workspaceConnection}
          workspaceConnectionAttempt={workspaceConnection === "live"
            ? podState.connectionAttempt
            : workspaceConnectionAttempt}
          chats={mobileChats}
          creatingChat={startingChat}
          chatView={agentView}
          approvalsView={(
            <MobileRepositoryApprovals
              workspace={workspace}
              approvals={repositoryApprovalState.value?.requests ?? []}
              podTitlesById={podState.value?.podTitlesById}
              loadError={repositoryApprovalState.error?.message}
            />
          )}
          approvalCount={(repositoryApprovalState.value?.requests ?? []).filter(
            (approval) => approval.status.tag === "Pending" || approval.status.tag === "Failed",
          ).length}
          error={visibleError}
          changesView={selectedPod
            ? (
              <MobileChangesView
                workspace={workspace}
                pod={selectedPod}
                review={route.changeReview
                  ? {
                      repository: route.changeReview.repository,
                      base: route.changeReview.base as changes.GitObjectId,
                      head: route.changeReview.head as changes.GitObjectId,
                    }
                  : undefined}
              />
            )
            : null}
          onSelectPod={selectPod}
          onCreatePod={togglePodCreation}
          onStartPod={async (podId) => {
            await guestApi(workspace).execute("pods_Start", { podId });
          }}
          onStopPod={async (podId) => {
            await guestApi(workspace).execute("pods_Stop", { podId });
          }}
          onDestroyPod={async (podId) => {
            await guestApi(workspace).execute("pods_Destroy", { podId });
          }}
          onSelectChat={selectWorkspaceChat}
          onNewChat={newChat}
        />
      ) : (
        <Suspense fallback={<UnavailablePanel detail="Loading the desktop workbench…" />}>
          <RepositoryApprovalOverlay
            key={workspace}
            workspace={workspace}
            approvals={repositoryApprovalState.value?.requests ?? []}
            podTitlesById={podState.value?.podTitlesById}
            onError={reportError}
          />
          <CodeDirectoryPalette
            open={codeDirectoryPaletteOpen}
            repositories={repositories}
            onOpenChange={setCodeDirectoryPaletteOpen}
            onSelect={(folder) => {
              if (selectedPodId) setCodeSelection({ podId: selectedPodId, folder });
            }}
          />
          <WorkbenchShell
            workspaces={allWorkspaces}
            selectedWorkspace={workspace}
            usbEnabled={configState.value?.config?.features?.usb ?? false}
            pods={pods}
            podChangeSummaries={podChangeSummaries}
            podChangeSummariesVerified={repositoryStatusState.ready
              && !repositoryStatusState.error}
            selectedPodId={selectedPodId}
            view={route.view}
            workspaceScreen={podCreationScreen ?? podStartupScreen}
            workspaceConnection={workspaceConnection === "live"
              ? podState.connection
              : workspaceConnection}
            workspaceConnectionAttempt={workspaceConnection === "live"
              ? podState.connectionAttempt
              : workspaceConnectionAttempt}
            onSelectWorkspace={onSelectWorkspace}
            onCreateWorkspace={onCreateWorkspace}
            onStartWorkspace={onStartWorkspace}
            onStopWorkspace={onStopWorkspace}
            onSelectPod={selectPod}
            canCreatePod
            podCreationActive={route.creatingPod}
            onCreatePod={togglePodCreation}
            onStartPod={async (podId) => {
              await guestApi(workspace).execute("pods_Start", { podId });
            }}
            onStopPod={async (podId) => {
              await guestApi(workspace).execute("pods_Stop", { podId });
            }}
            onDestroyPod={async (podId) => {
              await guestApi(workspace).execute("pods_Destroy", { podId });
            }}
            agentView={agentView}
            codeView={(
              <DesktopPanel loadingDetail="Loading the code view…">
                {selectedPod
                  ? selectedCodeFolder
                    ? (
                      <CodeView
                        workspace={workspace}
                        pod={selectedPod}
                        folder={selectedCodeFolder}
                      />
                    )
                    : (
                      <CodeDirectoryPicker
                        repositories={repositories}
                        onSelect={(folder) => setCodeSelection({
                          podId: selectedPod.id,
                          folder,
                        })}
                      />
                    )
                  : <UnavailablePanel detail="Select a pod to start its code editor." />}
              </DesktopPanel>
            )}
            changesView={(
              <DesktopPanel loadingDetail="Loading changes…">
                {selectedPod
                  ? <DesktopChangesView workspace={workspace} pod={selectedPod} />
                  : <UnavailablePanel detail="Select a pod to inspect its changes." />}
              </DesktopPanel>
            )}
            filesView={(
              <DesktopPanel loadingDetail="Loading files…">
                {selectedPod
                  ? <FilesView workspace={workspace} pod={selectedPod} />
                  : <UnavailablePanel detail="Select a pod to browse its files." />}
              </DesktopPanel>
            )}
            codeTabs={codeTabs}
            selectedCodeFolder={selectedPod ? selectedCodeFolder : undefined}
            onSelectCodeSession={(folder) => {
              if (selectedPodId) setCodeSelection({ podId: selectedPodId, folder });
            }}
            onNewCodeSession={() => setCodeDirectoryPaletteOpen(true)}
            onCloseCodeSession={closeCodeSession}
            imagesView={(
              <DesktopPanel loadingDetail="Loading images…">
                <ImagesView workspace={workspace} />
              </DesktopPanel>
            )}
            networkView={(
              <DesktopPanel loadingDetail="Loading network controls…">
                <NetworkView
                  workspace={workspace}
                  pods={pods}
                  podTitlesById={podState.value?.podTitlesById}
                />
              </DesktopPanel>
            )}
            repositoriesView={(
              <DesktopPanel loadingDetail="Loading repositories…">
                <RepositoriesView workspace={workspace} />
              </DesktopPanel>
            )}
            operationsView={(
              <DesktopPanel loadingDetail="Loading host operations…">
                <HostOperationsView workspace={workspace} />
              </DesktopPanel>
            )}
            publishedWebPreviews={publishedWebPreviews}
            publishedWebPreviewsReady={httpRouteState.ready}
            retainedPublishedWebPreviewIds={retainedPublishedWebPreviewIds}
            settingsView={(
              <DesktopPanel loadingDetail="Loading settings…">
                {currentWorkspace
                  ? <WorkspaceSettings workspace={currentWorkspace} />
                  : <UnavailablePanel detail="Settings are unavailable." />}
              </DesktopPanel>
            )}
            podProcessView={selectedPod
              ? (
                <DesktopPanel loadingDetail="Loading processes…">
                  <ProcessManager workspace={workspace} pod={selectedPod} />
                </DesktopPanel>
              )
              : undefined}
            repositories={repositories}
            repositoriesReady={repositoryState.ready && !repositoryState.error}
            repositoriesError={repositoryState.error?.message}
            repositoryStatuses={repositoryStatusState.value?.repositories ?? []}
            repositoryStatusesReady={repositoryStatusState.ready
              && !repositoryStatusState.error}
            agentTabs={agentTabs}
            selectedAgentId={startingChat ? undefined : selectedSummary?.chatId}
            creatingAgent={startingChat}
            busyPodIds={busyPodIds}
            attentionPodIds={attentionPodIds}
            agentNeedsInput={selectedSummary?.agentStatus === "UserInputRequired"}
            repositoryApprovalCount={repositoryApprovalState.value?.requests.length}
            hostOperationApprovalCount={(hostOperationState.value?.operations ?? []).filter(
              (operation) => operation.state.status === "AwaitingApproval",
            ).length}
            onSelectAgent={(chatId) => selectChat(chatId as chats.ChatId)}
            onNewAgent={newChat}
            onArchiveAgent={(chatId) => {
              const target = podChats.find((candidate) => candidate.chatId === chatId);
              if (target) setArchiveTarget(target);
            }}
            terminalView={(
              <DesktopPanel loadingDetail="Loading terminal…">
                {activeTerminal
                  ? <ProcessTerminal workspace={workspace} process={activeTerminal} />
                  : <UnavailablePanel detail="Select a terminal." />}
              </DesktopPanel>
            )}
            terminalTabs={terminalTabs}
            terminalTabsReady={processState.ready && selectedPodId !== undefined}
            activeTerminalId={activeTerminal?.id}
            onSelectTerminal={(processId) =>
              setActiveTerminalId(processId as processes.ProcessId)}
            onNewTerminal={newTerminal}
            onCloseTerminal={closeTerminal}
          />
        </Suspense>
      )}
    </>
  );
}

function httpRoutePreviews(
  routes: readonly network.HttpRoute[],
  podId: pods.PodId | undefined,
): WebPreview[] {
  if (!podId) return [];
  return routes
    .filter((route) => route.podId === podId && !route.internal)
    .map((route) => ({
      id: httpRoutePreviewId(route),
      title: route.title,
      url: "",
      hostnamePrefix: route.hostnamePrefix,
    }));
}

function httpRoutePreviewId(route: network.HttpRoute): string {
  return `http-route-${route.id}`;
}

function codeSessionTabs(
  podId: pods.PodId | undefined,
  selectedFolder: string | undefined,
  sessions: readonly code.CodeSession[],
  configuredRepositories: readonly repositories.Repository[],
): CodeWorkbenchTab[] {
  if (!podId) return [];
  const sessionsByFolder = new Map(sessions.map((session) => [session.folder, session]));
  const folders = [
    ...sessions.map((session) => session.folder),
    ...(selectedFolder && !sessionsByFolder.has(selectedFolder) ? [selectedFolder] : []),
  ].filter((folder, index, all) => all.indexOf(folder) === index);
  return folders
    .toSorted((left, right) => {
      if (left === DEFAULT_CODE_FOLDER) return -1;
      if (right === DEFAULT_CODE_FOLDER) return 1;
      return left.localeCompare(right);
    })
    .map((folder) => {
      const session = sessionsByFolder.get(folder);
      return {
        folder,
        title: codeFolderLabel(folder, configuredRepositories),
        status: session ? codeTabStatus(session.status) : "starting",
        closeable: folder !== DEFAULT_CODE_FOLDER && session !== undefined,
      };
    });
}

function codeTabStatus(status: code.CodeSessionStatus): CodeWorkbenchTab["status"] {
  switch (status.status) {
    case "Starting": return "starting";
    case "Running": return "running";
    case "Exited": return "exited";
    case "Failed": return "failed";
  }
}

export function UnavailableWorkspace({
  workspaces: availableWorkspaces,
  selectedWorkspace,
  screen,
  view,
  connection,
  connectionAttempt,
  error,
  onSelectWorkspace,
  onCreateWorkspace,
  onStartWorkspace,
  onStopWorkspace,
}: {
  workspaces: readonly workspaces.Workspace[];
  selectedWorkspace?: workspaces.Workspace;
  screen?: WorkspaceScreenName;
  view: WorkbenchRoute["view"];
  connection: WorkspaceConnection;
  connectionAttempt: number;
  error?: Error;
  onSelectWorkspace: (workspace: workspaces.WorkspaceName) => void;
  onCreateWorkspace: () => void;
  onStartWorkspace: (workspace: workspaces.WorkspaceName) => Promise<void>;
  onStopWorkspace: (workspace: workspaces.WorkspaceName) => Promise<void>;
}) {
  const mobileLayout = useMobileLayout();
  const activeScreen = screen ?? (selectedWorkspace
    ? workspaceScreenForState(selectedWorkspace.state)
    : undefined);
  const detail = error?.message ?? "Waiting for the workspace inventory.";
  const lifecycleScreen = selectedWorkspace && activeScreen ? (
    <WorkspaceLifecycleScreen
      screen={activeScreen}
      workspace={selectedWorkspace}
      onStart={() => onStartWorkspace(selectedWorkspace.name)}
    />
  ) : <UnavailablePanel detail={detail} />;
  if (mobileLayout && selectedWorkspace) {
    return (
      <MobileWorkspaceStatus
        workspace={selectedWorkspace.name}
        connection={connection}
        connectionAttempt={connectionAttempt}
      >
        {lifecycleScreen}
      </MobileWorkspaceStatus>
    );
  }
  return (
    <Suspense fallback={<UnavailablePanel detail="Loading the desktop workbench…" />}>
      <WorkbenchShell
        workspaces={availableWorkspaces}
        selectedWorkspace={selectedWorkspace?.name}
        usbEnabled={false}
        pods={[]}
        podChangeSummaries={EMPTY_POD_CHANGE_SUMMARIES}
        podChangeSummariesVerified={false}
        podListEmptyMessage={activeScreen ? `Workspace ${activeScreen}.` : undefined}
        showPodCount={false}
        view={view}
        workspaceConnection={connection}
        workspaceConnectionAttempt={connectionAttempt}
        workspaceScreen={view === "operations" ? undefined : lifecycleScreen}
        onSelectWorkspace={onSelectWorkspace}
        onCreateWorkspace={onCreateWorkspace}
        onStartWorkspace={onStartWorkspace}
        onStopWorkspace={onStopWorkspace}
        onSelectPod={() => undefined}
        canCreatePod={false}
        onCreatePod={() => undefined}
        onStartPod={async () => undefined}
        onStopPod={async () => undefined}
        onDestroyPod={async () => undefined}
        agentView={null}
        codeView={null}
        changesView={null}
        filesView={null}
        codeTabs={[]}
        onSelectCodeSession={() => undefined}
        onNewCodeSession={() => undefined}
        onCloseCodeSession={() => undefined}
        imagesView={null}
        networkView={null}
        repositoriesView={null}
        operationsView={selectedWorkspace
          ? <HostOperationsView workspace={selectedWorkspace.name} />
          : null}
        settingsView={null}
        publishedWebPreviewsReady={false}
        agentTabs={[]}
        onSelectAgent={() => undefined}
        onNewAgent={() => undefined}
        onArchiveAgent={() => undefined}
        terminalView={null}
        terminalTabs={[]}
        terminalTabsReady={false}
        onSelectTerminal={() => undefined}
        onNewTerminal={() => undefined}
        onCloseTerminal={() => undefined}
      />
    </Suspense>
  );
}

function SelectedChat({
  workspace,
  summary,
  harness,
  modelPreferences,
  usageSettings,
  slashCommands,
  attachmentUploader,
  attachmentUrl,
  onError,
}: {
  workspace: workspaces.WorkspaceName;
  summary: chats.ChatSummary;
  harness?: chats.ChatHarness;
  modelPreferences?: config.WorkspaceChatModelPreferences;
  usageSettings?: config.WorkspaceUsageSettings;
  slashCommands?: config.WorkspaceChatConfig["commands"];
  attachmentUploader: (file: File) => Promise<chats.ChatPromptAttachment>;
  attachmentUrl: (attachmentId: chats.ChatAttachmentId) => string;
  onError: (cause: unknown) => void;
}) {
  const chatState = useChat(workspace, summary.chatId);
  const currentSummary = chatState.value?.summary ?? summary;
  return (
    <ChatScreen
      key={summary.chatId}
      summary={currentSummary}
      replica={chatState.value}
      harness={harness}
      modelPreferences={modelPreferences}
      usageSettings={usageSettings}
      slashCommands={slashCommands}
      attachmentUploader={attachmentUploader}
      attachmentUrl={attachmentUrl}
      status={chatState.connection === "idle" ? "stopped" : chatState.connection}
      actions={chatActions(workspace, summary.chatId)}
      onError={onError}
    />
  );
}

function UnavailablePanel({ detail }: { detail: string }) {
  return (
    <div className="flex h-full items-center justify-center p-8 text-center text-sm text-muted">
      {detail}
    </div>
  );
}

function DesktopPanel({
  children,
  loadingDetail,
}: {
  children: ReactNode;
  loadingDetail: string;
}) {
  return (
    <Suspense fallback={<UnavailablePanel detail={loadingDetail} />}>
      {children}
    </Suspense>
  );
}

function InlineError({ message, onClose }: { message: string; onClose: () => void }) {
  return (
    <div className="mx-auto flex max-w-4xl items-start justify-between gap-4 text-xs text-red-200" role="alert">
      <span className="leading-5">{message}</span>
      <Button
        aria-label="Dismiss error"
        className="size-6 shrink-0 border-0 bg-transparent p-0 text-red-300/60 hover:text-red-200"
        size="icon"
        onClick={onClose}
      >
        <X aria-hidden="true" className="size-3.5" />
      </Button>
    </div>
  );
}

function createChat(
  workspace: workspaces.WorkspaceName,
  podId: pods.PodId,
  submission: StartChatSubmission,
) {
  return guestApi(workspace).execute("chats_Create", {
    podId,
    harness: submission.harness,
    ...(submission.title ? { title: submission.title } : {}),
    ...(submission.costCenterId ? { costCenterId: submission.costCenterId } : {}),
    ...(submission.model ? { model: submission.model } : {}),
    initialPrompt: submission.prompt,
    autoAttach: true,
  });
}

function createPod(
  workspace: workspaces.WorkspaceName,
  title: string,
) {
  return guestApi(workspace).execute("pods_Create", { title });
}

function createPodChat(
  workspace: workspaces.WorkspaceName,
  submission: StartChatSubmission,
) {
  return guestApi(workspace).execute("chats_CreatePodChat", {
    harness: submission.harness,
    ...(submission.title ? { title: submission.title } : {}),
    ...(submission.costCenterId ? { costCenterId: submission.costCenterId } : {}),
    ...(submission.model ? { model: submission.model } : {}),
    initialPrompt: submission.prompt,
  });
}

function terminalStatus(process: processes.Process): TerminalWorkbenchTab["status"] {
  if (process.status.status === "Failed") return "failed";
  if (process.status.status === "Exited") return "exited";
  return "running";
}

function withoutValue(values: ReadonlySet<string>, value: string): ReadonlySet<string> {
  const next = new Set(values);
  next.delete(value);
  return next;
}

function chatActions(workspace: workspaces.WorkspaceName, chatId: chats.ChatId) {
  const api = guestApi(workspace);
  return {
    sendPrompt: ({ prompt, mode }: PromptSubmission) =>
      api.execute("chats_SendPrompt", { chatId, prompt, mode }),
    interrupt: async () => {
      await api.execute("chats_Interrupt", { chatId });
    },
    compactContext: async () => {
      await api.execute("chats_CompactContext", { chatId });
    },
    attach: async () => {
      await api.execute("chats_AttachBinding", { chatId });
    },
    detach: async () => {
      await api.execute("chats_DetachBinding", { chatId });
    },
    archive: async () => {
      await api.execute("chats_Archive", { chatId });
    },
    setCostCenter: async (costCenterId?: chats.ChatCostCenterId) => {
      await api.execute("chats_SetCostCenter", {
        chatId,
        ...(costCenterId ? { costCenterId } : {}),
      });
    },
    flushPromptQueue: async () => {
      await api.execute("chats_FlushPromptQueue", { chatId });
    },
    removeQueuedPrompt: async (queuedPromptId: chats.ChatQueuedPromptId) => {
      await api.execute("chats_RemoveQueuedPrompt", { chatId, queuedPromptId });
    },
    resolveRequest: async (requestId: chats.ChatRequestId, answers: chats.ChatQuestionAnswer[]) => {
      await api.execute("chats_ResolveRequest", { chatId, requestId, answers });
    },
  };
}

function agentTabStatus(summary: chats.ChatSummary): AgentWorkbenchTab["status"] {
  if (summary.agentStatus === "Working") return "working";
  if (summary.agentStatus === "UserInputRequired") return "needs-input";
  if (summary.lastBindingError) return "failed";
  if (summary.binding?.status === "Attached") return "connected";
  return undefined;
}

const DEFAULT_TERMINAL_SIZE: processes.ProcessTerminal = {
  rows: 24 as processes.ProcessTerminal["rows"],
  cols: 80 as processes.ProcessTerminal["cols"],
};
