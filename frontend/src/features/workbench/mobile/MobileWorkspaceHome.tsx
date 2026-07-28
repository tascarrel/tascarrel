import { useNavigate } from "@tanstack/react-router";
import { ArrowLeft, ArrowRight } from "lucide-react";
import type { ReactNode } from "react";

import type { chats, pods, workspaces } from "../../../api/generated/index.ts";
import { Badge, type BadgeTone } from "../../../components/ui/Badge.tsx";
import { ConnectionOverlay } from "../../../components/ui/ConnectionOverlay.tsx";
import { TascarrelLogo } from "../../../components/ui/TascarrelLogo.tsx";
import { useChatList } from "../../chat/state.ts";
import { usePods } from "../../pods/state.ts";
import { MobileRepositoryApprovals } from "../../repositories/MobileRepositoryApprovals.tsx";
import { useRepositoryApprovals } from "../../repositories/state.ts";
import { MobileChatRow, mobileChatSummary } from "./MobilePodList.tsx";

export function MobileWorkspaceHome({
  workspaces: availableWorkspaces,
  onSelectWorkspace,
}: {
  workspaces: readonly workspaces.Workspace[];
  onSelectWorkspace: (workspace: workspaces.WorkspaceName) => void;
}) {
  const navigate = useNavigate();

  return (
    <main className="mobile-client">
      <header className="mobile-app-header">
        <span className="flex min-w-0 items-center gap-3">
          <TascarrelLogo className="size-8 shrink-0" />
          <span>
            <span className="block text-sm font-semibold text-foreground">Tascarrel</span>
            <span className="block text-[10px] text-subtle">Mobile pod client</span>
          </span>
        </span>
      </header>

      <div className="mobile-client-content min-h-0 flex-1 overflow-y-auto pt-5">
        <div className="mx-auto w-full min-w-0 max-w-2xl">
          <div className="mb-5">
            <h1 className="text-xl font-semibold tracking-tight text-foreground">Workspaces</h1>
            <p className="mt-1 text-sm leading-6 text-muted">
              Choose where you want to start or continue a pod.
            </p>
          </div>

          <div className="mb-6 grid gap-5">
            {availableWorkspaces
              .filter((workspace) => workspace.state.status === "Running")
              .map((workspace) => (
                <MobileWorkspaceActivity
                  key={workspace.name}
                  workspace={workspace.name}
                  onOpenChat={(podId, chatId) => {
                    void navigate({
                      to: "/workspaces/$workspace/pods/$pod/chats/$chat",
                      params: {
                        workspace: workspace.name,
                        pod: podId,
                        chat: chatId,
                      },
                      search: {},
                    });
                  }}
                />
              ))}
          </div>

          <div className="grid gap-3">
            {availableWorkspaces.map((workspace) => (
              <button
                className="flex min-h-20 w-full min-w-0 max-w-full items-center gap-3 overflow-hidden rounded-2xl border border-ui-border bg-surface/70 p-4 text-left transition active:bg-surface-raised"
                type="button"
                key={workspace.name}
                onClick={() => onSelectWorkspace(workspace.name)}
              >
                <span className="min-w-0 flex-1">
                  <span className="block truncate text-sm font-semibold text-foreground">
                    {workspace.name}
                  </span>
                  <span className="mt-1 block text-xs text-subtle">
                    {workspaceStateDetail(workspace.state.status)}
                  </span>
                </span>
                <Badge tone={workspaceStateTone(workspace.state.status)}>
                  {workspace.state.status}
                </Badge>
                <ArrowRight aria-hidden="true" className="size-4 shrink-0 text-subtle" />
              </button>
            ))}
          </div>
        </div>
      </div>
    </main>
  );
}

function MobileWorkspaceActivity({
  workspace,
  onOpenChat,
}: {
  workspace: workspaces.WorkspaceName;
  onOpenChat: (podId: pods.PodId, chatId: chats.ChatId) => void;
}) {
  const chatState = useChatList(workspace);
  const podState = usePods(workspace);
  const approvalState = useRepositoryApprovals(workspace);
  const workspaceChats = (chatState.value?.chats ?? []).map(mobileChatSummary);
  const attentionChats = workspaceChats.filter((chat) =>
    chat.attention || chat.status === "needs-input" || chat.status === "failed"
  );
  const workingChats = workspaceChats.filter((chat) =>
    chat.status === "working" && !attentionChats.includes(chat)
  );
  const approvals = approvalState.value?.requests ?? [];
  const pendingApprovalCount = approvals.filter(
    (approval) => approval.status.tag === "Pending" || approval.status.tag === "Failed",
  ).length;
  const loadError = chatState.error?.message
    ?? podState.error?.message
    ?? approvalState.error?.message;
  if (!attentionChats.length && !workingChats.length && !pendingApprovalCount && !loadError) {
    return null;
  }

  return (
    <section className="min-w-0 max-w-full" aria-label={`Activity in ${workspace}`}>
      <div className="mb-3 flex items-center justify-between gap-3">
        <div>
          <h2 className="text-sm font-semibold text-foreground">{workspace}</h2>
          <p className="mt-0.5 text-[10px] text-subtle">Live activity</p>
        </div>
        <Badge tone={attentionChats.length || pendingApprovalCount ? "warning" : "primary"}>
          {attentionChats.length + workingChats.length + pendingApprovalCount}
        </Badge>
      </div>

      <MobileRepositoryApprovals
        workspace={workspace}
        approvals={approvals}
        podTitlesById={podState.value?.podTitlesById}
        loadError={approvalState.error?.message}
      />

      {attentionChats.length || workingChats.length ? (
        <div className={`${pendingApprovalCount ? "mt-5" : ""} grid gap-2`}>
          {[...attentionChats, ...workingChats].map((chat) => (
            <MobileChatRow
              chat={chat}
              key={chat.id}
              podTitle={podState.value?.podTitlesById?.get(chat.podId) ?? "Unknown pod"}
              onClick={() => onOpenChat(chat.podId, chat.id)}
            />
          ))}
        </div>
      ) : null}
      {loadError && loadError !== approvalState.error?.message ? (
        <p className="mt-3 text-xs leading-5 text-red-200" role="alert">{loadError}</p>
      ) : null}
    </section>
  );
}

export function MobileWorkspaceStatus({
  workspace,
  connection,
  connectionAttempt,
  children,
}: {
  workspace: workspaces.WorkspaceName;
  connection: "idle" | "connecting" | "live" | "reconnecting";
  connectionAttempt: number;
  children: ReactNode;
}) {
  const navigate = useNavigate();
  return (
    <main className="mobile-client">
      <ConnectionOverlay connection={connection} attempt={connectionAttempt} />
      <header className="mobile-app-header">
        <button
          className="flex size-11 shrink-0 items-center justify-center rounded-xl text-muted active:bg-surface-raised active:text-foreground"
          type="button"
          aria-label="All workspaces"
          onClick={() => void navigate({ to: "/", search: {} })}
        >
          <ArrowLeft aria-hidden="true" className="size-5" />
        </button>
        <div className="min-w-0 flex-1">
          <h1 className="truncate text-sm font-semibold text-foreground">{workspace}</h1>
          <p className="truncate text-[10px] text-subtle">Workspace status</p>
        </div>
      </header>
      <div className="min-h-0 flex-1 overflow-auto">{children}</div>
    </main>
  );
}

function workspaceStateTone(status: workspaces.WorkspaceState["status"]): BadgeTone {
  if (status === "Running") return "success";
  if (status === "Failed") return "danger";
  if (status === "Starting" || status === "Stopping" || status === "Destroying") return "warning";
  return "muted";
}

function workspaceStateDetail(status: workspaces.WorkspaceState["status"]): string {
  if (status === "Running") return "Ready for pods";
  if (status === "Stopped") return "Starts when opened";
  if (status === "Failed") return "Needs attention";
  return `Workspace is ${status.toLowerCase()}`;
}
