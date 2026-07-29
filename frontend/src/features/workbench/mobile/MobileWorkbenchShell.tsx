import { useNavigate } from "@tanstack/react-router";
import { ArrowLeft, Bell } from "lucide-react";
import { useState, type ReactNode } from "react";

import type { chats, pods, workspaces } from "../../../api/generated/index.ts";
import type { WorkbenchRoute } from "../../../app/router.tsx";
import { Badge } from "../../../components/ui/Badge.tsx";
import { ConnectionOverlay } from "../../../components/ui/ConnectionOverlay.tsx";
import { CountBadge } from "../../../components/ui/CountBadge.tsx";
import type { PodChangeSummary } from "../../changes/podChangeSummary.ts";
import { PodDestructionDialog } from "../../pods/PodDestructionDialog.tsx";
import { MobilePodScreen } from "./MobilePodScreen.tsx";
import type { MobileChatSummary } from "./MobilePodList.tsx";
import { MobileWorkspaceScreen } from "./MobileWorkspaceScreen.tsx";

export function MobileWorkbenchShell({
  workspaces: availableWorkspaces,
  selectedWorkspace,
  pods: workspacePods,
  podChangeSummaries,
  podChangeSummariesVerified,
  selectedPodId,
  selectedChatId,
  route,
  workspaceConnection,
  workspaceConnectionAttempt,
  workspaceScreen,
  chats: workspaceChats,
  creatingChat,
  chatView,
  approvalsView,
  approvalCount,
  changesView,
  error,
  onSelectPod,
  onCreatePod,
  onStartPod,
  onStopPod,
  onDestroyPod,
  onSelectChat,
  onNewChat,
}: {
  workspaces: readonly workspaces.Workspace[];
  selectedWorkspace: workspaces.WorkspaceName;
  pods: readonly pods.Pod[];
  podChangeSummaries: ReadonlyMap<pods.PodId, PodChangeSummary>;
  podChangeSummariesVerified: boolean;
  selectedPodId?: pods.PodId;
  selectedChatId?: chats.ChatId;
  route: WorkbenchRoute;
  workspaceConnection: "idle" | "connecting" | "live" | "reconnecting";
  workspaceConnectionAttempt: number;
  workspaceScreen?: ReactNode;
  chats: readonly MobileChatSummary[];
  creatingChat: boolean;
  chatView: ReactNode;
  approvalsView: ReactNode;
  approvalCount: number;
  changesView: ReactNode;
  error?: string;
  onSelectPod: (podId: pods.PodId) => void;
  onCreatePod: () => void;
  onStartPod: (podId: pods.PodId) => Promise<void>;
  onStopPod: (podId: pods.PodId) => Promise<void>;
  onDestroyPod: (podId: pods.PodId) => Promise<void>;
  onSelectChat: (podId: pods.PodId, chatId: chats.ChatId) => void;
  onNewChat: () => void;
}) {
  const navigate = useNavigate();
  const selectedPod = workspacePods.find((pod) => pod.id === selectedPodId);
  const selectedChat = workspaceChats.find((chat) => chat.id === selectedChatId);
  const [pendingPodAction, setPendingPodAction] = useState<PendingPodAction>();
  const [destroyTarget, setDestroyTarget] = useState<pods.Pod>();
  const [podActionError, setPodActionError] = useState<string>();
  const workspaceExists = availableWorkspaces.some(
    (workspace) => workspace.name === selectedWorkspace,
  );
  const podChats = selectedPodId
    ? workspaceChats.filter((chat) => chat.podId === selectedPodId)
    : [];

  const openWorkspace = () => {
    void navigate({
      to: "/workspaces/$workspace",
      params: { workspace: selectedWorkspace },
      search: {},
    });
  };
  const openWorkspaces = () => {
    void navigate({ to: "/", search: {} });
  };
  const openChanges = (podId: pods.PodId) => {
    void navigate({
      to: "/workspaces/$workspace/pods/$pod/changes",
      params: { workspace: selectedWorkspace, pod: podId },
      search: {},
    });
  };
  const runPodAction = async (pod: pods.Pod, operation: PendingPodAction["operation"]) => {
    if (pendingPodAction) return;
    setPendingPodAction({ operation, podId: pod.id });
    setPodActionError(undefined);
    try {
      if (operation === "start") await onStartPod(pod.id);
      else if (operation === "stop") await onStopPod(pod.id);
      else {
        await onDestroyPod(pod.id);
        setDestroyTarget(undefined);
        openWorkspace();
      }
    } catch (cause) {
      setPodActionError(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setPendingPodAction(undefined);
    }
  };

  const navigation = mobileNavigation({
    route,
    selectedWorkspace,
    selectedPod,
    selectedChat,
    creatingChat,
    openWorkspace,
    openWorkspaces,
    onSelectPod,
  });
  const content = workspaceScreen ? (
    <div className="min-h-0 flex-1 overflow-hidden">{workspaceScreen}</div>
  ) : route.chat || creatingChat ? (
    <div className="min-h-0 flex-1 overflow-hidden">{chatView}</div>
  ) : route.view === "changes" && selectedPod ? (
    <div className="min-h-0 flex-1 overflow-hidden">{changesView}</div>
  ) : isDesktopOnlyView(route.view) ? (
    <MobileDesktopOnlyView />
  ) : selectedPod ? (
    <MobilePodScreen
      pod={selectedPod}
      chats={podChats}
      changeSummary={podChangeSummaries.get(selectedPod.id)}
      pendingOperation={pendingPodAction?.podId === selectedPod.id
        ? pendingPodAction.operation
        : undefined}
      actionError={podActionError}
      onSelectChat={(chatId) => onSelectChat(selectedPod.id, chatId)}
      onNewChat={onNewChat}
      onOpenChanges={() => openChanges(selectedPod.id)}
      onStart={() => void runPodAction(selectedPod, "start")}
      onStop={() => void runPodAction(selectedPod, "stop")}
      onDelete={() => {
        setPodActionError(undefined);
        setDestroyTarget(selectedPod);
      }}
    />
  ) : (
    <MobileWorkspaceScreen
      pods={workspacePods}
      chats={workspaceChats}
      podChangeSummaries={podChangeSummaries}
      approvalsView={approvalsView}
      onCreatePod={onCreatePod}
      onSelectPod={onSelectPod}
    />
  );

  return (
    <>
      <PodDestructionDialog
        action="delete"
        error={podActionError}
        pending={destroyTarget
          ? pendingPodAction?.operation === "destroy"
            && pendingPodAction.podId === destroyTarget.id
          : false}
        pod={destroyTarget}
        summary={destroyTarget ? podChangeSummaries.get(destroyTarget.id) : undefined}
        verified={podChangeSummariesVerified}
        onOpenChange={(open) => {
          if (!open) {
            setDestroyTarget(undefined);
            setPodActionError(undefined);
          }
        }}
        onConfirm={() => {
          if (destroyTarget) void runPodAction(destroyTarget, "destroy");
        }}
      />
      <main className="mobile-client">
        <ConnectionOverlay connection={workspaceConnection} attempt={workspaceConnectionAttempt} />
        <header className="mobile-app-header">
          <button
            className="flex size-11 shrink-0 items-center justify-center rounded-xl text-muted active:bg-surface-raised active:text-foreground"
            type="button"
            aria-label={navigation.backLabel}
            onClick={navigation.onBack}
          >
            <ArrowLeft aria-hidden="true" className="size-5" />
          </button>
          <div className="min-w-0 flex-1">
            <h1 className="truncate text-sm font-semibold text-foreground">{navigation.title}</h1>
            <p className="truncate text-[10px] text-subtle">{navigation.detail}</p>
          </div>
          {!workspaceExists ? (
            <Badge tone="danger">Unavailable</Badge>
          ) : approvalCount > 0 && selectedPod ? (
            <button
              className="relative flex size-11 shrink-0 items-center justify-center rounded-xl text-amber-300 active:bg-amber-500/10"
              type="button"
              aria-label={`${approvalCount} publication ${approvalCount === 1 ? "approval" : "approvals"}`}
              onClick={openWorkspace}
            >
              <Bell aria-hidden="true" className="size-5" />
              <CountBadge
                className="absolute right-1.5 top-1.5"
                count={approvalCount}
                size="xs"
                tone="warning"
              />
            </button>
          ) : null}
        </header>
        {error && !workspaceScreen && !route.chat && !creatingChat ? (
          <p
            className="mobile-client-horizontal break-all border-b border-red-500/20 bg-red-500/5 py-2.5 text-xs leading-5 text-red-200"
            role="alert"
          >
            {error}
          </p>
        ) : null}
        {content}
      </main>
    </>
  );
}

type PendingPodAction = {
  operation: "start" | "stop" | "destroy";
  podId: pods.PodId;
};

type MobileNavigation = {
  title: string;
  detail: string;
  backLabel: string;
  onBack: () => void;
};

function mobileNavigation({
  route,
  selectedWorkspace,
  selectedPod,
  selectedChat,
  creatingChat,
  openWorkspace,
  openWorkspaces,
  onSelectPod,
}: {
  route: WorkbenchRoute;
  selectedWorkspace: workspaces.WorkspaceName;
  selectedPod?: pods.Pod;
  selectedChat?: MobileChatSummary;
  creatingChat: boolean;
  openWorkspace: () => void;
  openWorkspaces: () => void;
  onSelectPod: (podId: pods.PodId) => void;
}): MobileNavigation {
  if (selectedPod) {
    return {
      title: selectedPod.title || "Untitled pod",
      detail: selectedChat
        ? selectedChat.title
        : route.view === "changes"
          ? "Changed files"
          : creatingChat
            ? "New chat"
            : "Pod",
      backLabel: "Workspace",
      onBack: route.chat || creatingChat || route.view === "changes"
        ? () => onSelectPod(selectedPod.id)
        : openWorkspace,
    };
  }
  return {
    title: String(selectedWorkspace),
    detail: route.creatingPod ? "New pod" : "Pod list",
    backLabel: route.creatingPod ? "Workspace" : "All workspaces",
    onBack: route.creatingPod ? openWorkspace : openWorkspaces,
  };
}

function MobileDesktopOnlyView() {
  return (
    <div className="flex min-h-0 flex-1 items-center justify-center p-6 text-center">
      <div className="max-w-sm">
        <h2 className="text-sm font-semibold text-foreground">Available on Desktop</h2>
        <p className="mt-2 text-xs leading-5 text-subtle">
          This workbench view is intentionally omitted from the mobile pod client.
        </p>
      </div>
    </div>
  );
}

function isDesktopOnlyView(view: WorkbenchRoute["view"]): boolean {
  return view === "code"
    || view === "files"
    || view === "images"
    || view === "network"
    || view === "repositories"
    || view === "operations"
    || view === "settings";
}
