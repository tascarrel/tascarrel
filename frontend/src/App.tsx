import { Outlet } from "@tanstack/react-router";
import { useEffect, useRef } from "react";

import { hostApi } from "./api/client.ts";
import type { workspaces } from "./api/generated/index.ts";
import { useMobileLayout } from "./app/layout.ts";
import { rootRoute, useWorkbenchRoute } from "./app/router.tsx";
import { ConnectionOverlay } from "./components/ui/ConnectionOverlay.tsx";
import {
  UnavailableWorkspace,
  WorkspaceWorkbench,
} from "./features/workbench/WorkspaceWorkbench.tsx";
import { MobileWorkspaceHome } from "./features/workbench/mobile/MobileWorkspaceHome.tsx";
import { WorkspaceCreationPage } from "./features/workspaces/creation/WorkspaceCreationPage.tsx";
import { useWorkspaces } from "./features/workspaces/state.ts";

export function App() {
  const route = useWorkbenchRoute();
  const mobileLayout = useMobileLayout();
  const navigate = rootRoute.useNavigate();
  const workspaceState = useWorkspaces();
  const workspaceRouteVisit = useRef<{ handled: boolean; workspace?: string }>({
    handled: false,
  });
  const lastSelectedWorkspace = useRef<workspaces.WorkspaceName | undefined>(undefined);
  const pendingCreatedWorkspace = useRef<workspaces.WorkspaceName | undefined>(undefined);
  const availableWorkspaces = workspaceState.value?.workspaces ?? [];
  const routedWorkspace = availableWorkspaces.find(
    (workspace) => workspace.name === route.workspace,
  );
  const fallbackWorkspace = availableWorkspaces.find(
    (workspace) => workspace.name === lastSelectedWorkspace.current,
  ) ?? availableWorkspaces[0];
  const selectedWorkspace = routedWorkspace
    ?? (mobileLayout && !route.creatingWorkspace ? undefined : fallbackWorkspace);

  useEffect(() => {
    if (routedWorkspace) lastSelectedWorkspace.current = routedWorkspace.name;
    if (routedWorkspace?.name === pendingCreatedWorkspace.current) {
      pendingCreatedWorkspace.current = undefined;
    }
  }, [routedWorkspace]);

  useEffect(() => {
    if (
      mobileLayout
      || route.globalScreen
      || route.creatingWorkspace
      || route.workspace === pendingCreatedWorkspace.current
      || !selectedWorkspace
      || route.workspace === selectedWorkspace.name
    ) return;
    void navigate({
      to: "/workspaces/$workspace",
      params: { workspace: selectedWorkspace.name },
      replace: true,
    });
  }, [
    mobileLayout,
    navigate,
    route.creatingWorkspace,
    route.globalScreen,
    route.workspace,
    selectedWorkspace,
  ]);

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
      search: {},
    });
  };

  const createWorkspaceAndOpen = async (input: workspaces.CreateWorkspaceAction) => {
    await hostApi.execute("workspaces_Create", input);
    pendingCreatedWorkspace.current = input.name;
    await navigate({
      to: "/workspaces/$workspace",
      params: { workspace: input.name },
      search: {},
    });
  };

  const openWorkspaceCreation = () => {
    void navigate({ to: "/workspaces/new", search: {} });
  };

  const cancelWorkspaceCreation = () => {
    if (!selectedWorkspace) return;
    void navigate({
      to: "/workspaces/$workspace",
      params: { workspace: selectedWorkspace.name },
      search: {},
    });
  };

  if (route.globalScreen) {
    if (route.globalScreen === "welcome") {
      return (
        <>
          <Outlet />
          <WorkspaceCreationPage
            canCancel={Boolean(selectedWorkspace)}
            onCancel={cancelWorkspaceCreation}
            onCreateWorkspace={createWorkspaceAndOpen}
          />
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

  if (
    route.creatingWorkspace
    || (workspaceState.ready && availableWorkspaces.length === 0)
  ) {
    return (
      <>
        <Outlet />
        <ConnectionOverlay
          connection={workspaceState.connection}
          attempt={workspaceState.connectionAttempt}
        />
        <WorkspaceCreationPage
          canCancel={availableWorkspaces.length > 0}
          onCancel={cancelWorkspaceCreation}
          onCreateWorkspace={createWorkspaceAndOpen}
        />
      </>
    );
  }

  if (mobileLayout && !selectedWorkspace) {
    return (
      <>
        <Outlet />
        <ConnectionOverlay
          connection={workspaceState.connection}
          attempt={workspaceState.connectionAttempt}
        />
        <MobileWorkspaceHome
          workspaces={availableWorkspaces}
          onSelectWorkspace={selectWorkspace}
        />
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
          onCreateWorkspace={openWorkspaceCreation}
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
          onCreateWorkspace={openWorkspaceCreation}
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
