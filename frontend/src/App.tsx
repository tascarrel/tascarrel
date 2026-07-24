import { Outlet } from "@tanstack/react-router";
import { useEffect, useRef } from "react";

import { hostApi } from "./api/client.ts";
import type { workspaces } from "./api/generated/index.ts";
import { rootRoute, useWorkbenchRoute } from "./app/router.tsx";
import { ConnectionOverlay } from "./components/ui/ConnectionOverlay.tsx";
import {
  UnavailableWorkspace,
  WorkspaceWorkbench,
} from "./features/workbench/WorkspaceWorkbench.tsx";
import { WelcomeScreen } from "./features/workspaces/WelcomeScreen.tsx";
import { useWorkspaces } from "./features/workspaces/state.ts";

export function App() {
  const route = useWorkbenchRoute();
  const navigate = rootRoute.useNavigate();
  const workspaceState = useWorkspaces();
  const workspaceRouteVisit = useRef<{ handled: boolean; workspace?: string }>({
    handled: false,
  });
  const availableWorkspaces = workspaceState.value?.workspaces ?? [];
  const selectedWorkspace = availableWorkspaces.find(
    (workspace) => workspace.name === route.workspace,
  ) ?? availableWorkspaces[0];

  useEffect(() => {
    if (route.globalScreen || !selectedWorkspace || route.workspace === selectedWorkspace.name) return;
    void navigate({
      to: "/workspaces/$workspace",
      params: { workspace: selectedWorkspace.name },
      replace: true,
    });
  }, [navigate, route.globalScreen, route.workspace, selectedWorkspace]);

  useEffect(() => {
    if (workspaceRouteVisit.current.workspace !== route.workspace) {
      workspaceRouteVisit.current = {
        handled: false,
        workspace: route.workspace,
      };
    }

    if (
      !route.workspace
      || route.screen
      || selectedWorkspace?.name !== route.workspace
      || workspaceRouteVisit.current.handled
    ) return;

    if (selectedWorkspace.state.status === "Stopping") return;
    workspaceRouteVisit.current.handled = true;
    if (selectedWorkspace.state.status !== "Stopped") return;

    void hostApi.execute("workspaces_Start", { workspace: selectedWorkspace.name })
      .catch(() => undefined);
  }, [route.screen, route.workspace, selectedWorkspace]);

  const selectWorkspace = (workspace: workspaces.WorkspaceName) => {
    void navigate({
      to: "/workspaces/$workspace",
      params: { workspace },
    });
  };

  const createWorkspace = async (name: workspaces.WorkspaceName) => {
    await hostApi.execute("workspaces_Create", { name });
  };

  const createWorkspaceAndOpen = async (name: workspaces.WorkspaceName) => {
    await createWorkspace(name);
    await navigate({
      to: "/workspaces/$workspace",
      params: { workspace: name },
    });
  };

  if (route.globalScreen) {
    if (route.globalScreen === "welcome") {
      return (
        <>
          <Outlet />
          <WelcomeScreen onCreateWorkspace={createWorkspaceAndOpen} />
        </>
      );
    }
    return (
      <>
        <Outlet />
        <ConnectionOverlay
          connection={route.globalScreen}
          attempt={route.globalScreen === "connecting" ? 1 : 4}
        />
      </>
    );
  }

  if (workspaceState.ready && availableWorkspaces.length === 0) {
    return (
      <>
        <Outlet />
        <ConnectionOverlay
          connection={workspaceState.connection}
          attempt={workspaceState.connectionAttempt}
        />
        <WelcomeScreen onCreateWorkspace={createWorkspaceAndOpen} />
      </>
    );
  }

  return (
    <>
      <Outlet />
      {selectedWorkspace?.state.status === "Running" && !route.screen ? (
        <WorkspaceWorkbench
          key={selectedWorkspace.name}
          allWorkspaces={availableWorkspaces}
          workspace={selectedWorkspace.name}
          route={route}
          workspaceConnection={workspaceState.connection}
          workspaceConnectionAttempt={workspaceState.connectionAttempt}
          onSelectWorkspace={selectWorkspace}
          onCreateWorkspace={createWorkspace}
          onStartWorkspace={async (workspace) => {
            await hostApi.execute("workspaces_Start", { workspace });
          }}
          onStopWorkspace={async (workspace) => {
            await hostApi.execute("workspaces_Stop", { workspace });
          }}
        />
      ) : (
        <UnavailableWorkspace
          workspaces={availableWorkspaces}
          selectedWorkspace={selectedWorkspace}
          screen={route.screen}
          view={route.view}
          connection={workspaceState.connection}
          connectionAttempt={workspaceState.connectionAttempt}
          error={workspaceState.error}
          onSelectWorkspace={selectWorkspace}
          onCreateWorkspace={createWorkspace}
          onStartWorkspace={async (workspace) => {
            await hostApi.execute("workspaces_Start", { workspace });
          }}
          onStopWorkspace={async (workspace) => {
            await hostApi.execute("workspaces_Stop", { workspace });
          }}
        />
      )}
    </>
  );
}
