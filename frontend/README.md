# Tascarrel Frontend

This package contains the Tascarrel workbench and chat interface.

The frontend uses the v2 control-plane client and a provider-owned state cache. Backend-owned
state is keyed by its host or guest identity, so every workspace, pod list, chat list, harness
inventory, and chat detail has one cached replica and at most one physical subscription.

Chat operation names in `src/api/actions.ts` are maintained manually while the backend operation
registry is developed in parallel. Sidex types can be refreshed with:

```sh
nix develop --command pnpm run generate:types
```

Check and build the package with:

```sh
nix develop --command pnpm run typecheck
nix develop --command pnpm run build
```

## Developing Inside Tascarrel

Point browser API requests at the host Tascarrel daemon while Vite continues to serve the frontend
from its pod:

```sh
VITE_TASCARREL_API_ROOT=http://tascarrel.localhost:8272/api/v1 pnpm run dev
podctl http publish 5174 --title "Tascarrel frontend"
```

Open the packaged Tascarrel frontend, find the published route in the Network view, and choose
**Trust as Tascarrel frontend**. Tascarrel grants API access only to that route's exact browser origin;
other routes and previews nested beneath the development frontend remain untrusted.
