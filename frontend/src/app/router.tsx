import {
  createRootRoute,
  createRoute,
  createRouter,
  useMatchRoute,
} from "@tanstack/react-router";

import { App } from "../App.tsx";

export type WorkspaceView = "agent" | "code" | "changes" | "files" | "pod" | "images" | "network" | "repositories" | "operations" | "settings";
export type GlobalScreenName = "connecting" | "reconnecting" | "welcome";
export type WorkspaceScreenName = "destroying" | "failed" | "starting" | "stopped" | "stopping";

export type WorkbenchRoute = {
  workspace?: string;
  pod?: string;
  chat?: string;
  creatingWorkspace?: boolean;
  creatingPod?: boolean;
  globalScreen?: GlobalScreenName;
  screen?: WorkspaceScreenName;
  view: WorkspaceView;
  changeReview?: {
    repository: string;
    base: string;
    head: string;
  };
};

type WorkbenchSearch = {
  path?: string;
  repository?: string;
  base?: string;
  head?: string;
};

export const rootRoute = createRootRoute({
  component: App,
  validateSearch: (search: Record<string, unknown>): WorkbenchSearch => ({
    ...(typeof search.path === "string" && search.path ? { path: search.path } : {}),
    ...(typeof search.repository === "string" && search.repository
      ? { repository: search.repository }
      : {}),
    ...(typeof search.base === "string" && search.base ? { base: search.base } : {}),
    ...(typeof search.head === "string" && search.head ? { head: search.head } : {}),
  }),
});

const emptyRouteComponent = () => null;
const indexRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/",
  component: emptyRouteComponent,
});
const globalScreenRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/screens/$screen",
  component: emptyRouteComponent,
});
const createWorkspaceRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/workspaces/new",
  component: emptyRouteComponent,
});
const workspaceRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/workspaces/$workspace",
  component: emptyRouteComponent,
});
const workspaceScreenRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/workspace/$workspace/screens/$screen",
  component: emptyRouteComponent,
});
const workspacesScreenRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/workspaces/$workspace/screens/$screen",
  component: emptyRouteComponent,
});
const imagesRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/workspaces/$workspace/images",
  component: emptyRouteComponent,
});
const networkRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/workspaces/$workspace/network",
  component: emptyRouteComponent,
});
const repositoriesRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/workspaces/$workspace/repositories",
  component: emptyRouteComponent,
});
const operationsRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/workspaces/$workspace/operations",
  component: emptyRouteComponent,
});
const settingsRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/workspaces/$workspace/settings",
  component: emptyRouteComponent,
});
const createPodRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/workspaces/$workspace/pods/new",
  component: emptyRouteComponent,
});
const podRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/workspaces/$workspace/pods/$pod",
  component: emptyRouteComponent,
});
const chatRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/workspaces/$workspace/pods/$pod/chats/$chat",
  component: emptyRouteComponent,
});
const codeRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/workspaces/$workspace/pods/$pod/code",
  component: emptyRouteComponent,
});
const changesRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/workspaces/$workspace/pods/$pod/changes",
  component: emptyRouteComponent,
});
const filesRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/workspaces/$workspace/pods/$pod/files",
  component: emptyRouteComponent,
});
const podOverviewRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/workspaces/$workspace/pods/$pod/pod",
  component: emptyRouteComponent,
});

const routeTree = rootRoute.addChildren([
  indexRoute,
  globalScreenRoute,
  createWorkspaceRoute,
  workspaceRoute,
  workspaceScreenRoute,
  workspacesScreenRoute,
  imagesRoute,
  networkRoute,
  repositoriesRoute,
  operationsRoute,
  settingsRoute,
  createPodRoute,
  podRoute,
  chatRoute,
  codeRoute,
  changesRoute,
  filesRoute,
  podOverviewRoute,
]);

export const router = createRouter({
  routeTree,
  defaultPreload: "intent",
});

declare module "@tanstack/react-router" {
  interface Register {
    router: typeof router;
  }
}

export function useWorkbenchRoute(): WorkbenchRoute {
  const matchRoute = useMatchRoute();
  const search = rootRoute.useSearch();
  const globalScreen = matchRoute({ to: "/screens/$screen" });
  if (globalScreen && isGlobalScreenName(globalScreen.screen)) {
    return { globalScreen: globalScreen.screen, view: "agent" };
  }

  const createWorkspace = matchRoute({ to: "/workspaces/new" });
  if (createWorkspace) return { creatingWorkspace: true, view: "agent" };

  const screen = matchRoute({ to: "/workspaces/$workspace/screens/$screen" })
    ?? matchRoute({ to: "/workspace/$workspace/screens/$screen" });
  if (screen && isWorkspaceScreenName(screen.screen)) {
    return { ...screen, screen: screen.screen, view: "agent" };
  }

  const chat = matchRoute({ to: "/workspaces/$workspace/pods/$pod/chats/$chat" });
  if (chat) return { ...chat, view: "agent" };

  const createPod = matchRoute({ to: "/workspaces/$workspace/pods/new" });
  if (createPod) return { ...createPod, creatingPod: true, view: "agent" };

  const code = matchRoute({ to: "/workspaces/$workspace/pods/$pod/code" });
  if (code) return { ...code, view: "code" };

  const changes = matchRoute({ to: "/workspaces/$workspace/pods/$pod/changes" });
  if (changes) {
    const changeReview = search.repository && search.base && search.head
      ? {
          repository: search.repository,
          base: search.base,
          head: search.head,
        }
      : undefined;
    return { ...changes, view: "changes", ...(changeReview ? { changeReview } : {}) };
  }

  const files = matchRoute({ to: "/workspaces/$workspace/pods/$pod/files" });
  if (files) return { ...files, view: "files" };

  const podOverview = matchRoute({ to: "/workspaces/$workspace/pods/$pod/pod" });
  if (podOverview) return { ...podOverview, view: "pod" };

  const pod = matchRoute({ to: "/workspaces/$workspace/pods/$pod" });
  if (pod) return { ...pod, view: "agent" };

  const images = matchRoute({ to: "/workspaces/$workspace/images" });
  if (images) return { ...images, view: "images" };

  const network = matchRoute({ to: "/workspaces/$workspace/network" });
  if (network) return { ...network, view: "network" };

  const repositories = matchRoute({ to: "/workspaces/$workspace/repositories" });
  if (repositories) return { ...repositories, view: "repositories" };

  const operations = matchRoute({ to: "/workspaces/$workspace/operations" });
  if (operations) return { ...operations, view: "operations" };

  const settings = matchRoute({ to: "/workspaces/$workspace/settings" });
  if (settings) return { ...settings, view: "settings" };

  const workspace = matchRoute({ to: "/workspaces/$workspace" });
  return workspace ? { ...workspace, view: "agent" } : { view: "agent" };
}

function isGlobalScreenName(value: string): value is GlobalScreenName {
  return value === "connecting" || value === "reconnecting" || value === "welcome";
}

function isWorkspaceScreenName(value: string): value is WorkspaceScreenName {
  return value === "stopped"
    || value === "starting"
    || value === "stopping"
    || value === "failed"
    || value === "destroying";
}
