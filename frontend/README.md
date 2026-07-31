# Tascarrel Frontend

This package contains the Tascarrel workbench and chat interface.

The frontend uses the v2 control-plane client and a provider-owned state cache. Backend-owned
state is keyed by its host or guest identity, so every workspace, pod list, chat list, harness
inventory, and chat detail has one cached replica and at most one physical subscription.
Control-plane transport interruptions update shared connection state rather than feature error
state. Presentation shells show a connecting overlay while subscriptions resume; operation-specific
failures remain visible in their owning feature.

## Desktop and Mobile Clients

The frontend selects one of two presentation shells while sharing routes, backend state, and
feature components. The desktop workbench provides the multi-panel development environment. The
mobile client focuses on creating pods, monitoring chats, resolving input requests, approving
repository publications, managing pod lifecycles, and reviewing individual changed-file diffs.
Repository approval surfaces load introduced commits and their exact patches from immutable
host-retained Git objects rather than from a pod checkout.
Its workspace index subscribes to every running workspace and collects active chats and unblock
requests into one live attention view. Within a workspace, users open a pod before choosing or
starting a chat, with the most recently created pods listed first. Mobile pod screens stay within
the viewport, while intrinsically wide artifacts such as code and diffs scroll inside their dedicated
viewers.

Markdown file links in chat output resolve within the current pod's `/workspace` directory and open
without leaving the conversation. The preview overlay syntax-highlights text, offers rendered and
source representations for Markdown, and uses dedicated PDF and image viewers. Absolute links must
start with `/workspace/`; relative links in rendered Markdown resolve from the containing file.

Code editing, terminals, arbitrary file browsing, network configuration, image management, and
advanced settings remain desktop-only. Keep new workbench features out of the mobile client unless
they directly help a user create a pod or unblock ongoing work.

The API types under `src/api/generated` and the operation registry in
`src/api/actions.ts` are generated from the Sidex schemas and Rust operation
registry. Regenerate both outputs with:

```sh
nix develop --command pnpm run generate:types
```

Do not edit either generated output by hand.

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
