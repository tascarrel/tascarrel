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

Start Vite in the pod and publish it through Tascarrel:

```sh
pnpm run dev
podctl http publish --title "Tascarrel frontend" 5174
```

In the packaged Tascarrel frontend, find the published route in the Network view and choose
**Trust as Tascarrel frontend**, then open it with the route action. Hostd issues a one-time route
ticket, installs a route-scoped `HttpOnly` cookie, and exposes the Tascarrel API below
`/.tascarrel/api/v1` on that exact trusted route. The development frontend discovers this bridge
through `/.tascarrel/context`; no API-root environment override is needed.

Frontend trust remains exclusive. Trusting another route removes trust from the previous route.
Ordinary published routes receive only their proxied application traffic, and hostd strips its
route credential before forwarding requests into the pod.
