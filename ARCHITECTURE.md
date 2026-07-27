# Tascarrel Architecture

Tascarrel is a local development environment for people and coding agents. It
separates work at two levels:

- A **workspace** is a long-lived development context. It groups repositories,
  tools, configuration, and persistent resources, and runs in its own virtual
  machine.
- A **pod** is a disposable environment for one task within a workspace. It has
  its own processes and writable filesystem but shares the workspace virtual
  machine's kernel and any explicitly shared resources.

This distinction drives the architecture. The virtual machine is the security
boundary between a workspace and the user's physical machine (the **host**).
Pods are lighter-weight environments intended to make parallel tasks cheap,
not to isolate mutually hostile workloads from one another.

This document describes the current implementation. It first establishes the
components and trust boundaries, then follows control, process, storage,
network, secret, and repository flows through the system.

## System Model

```text
 Browser ── HTTP/WebSocket ──┐
                             ├──► Host daemon (`hostd`)
 CLI ───── Unix socket ──────┘          │
                                        │ one private connection per VM
                                        ▼
                              Workspace VM (one per workspace)
                              ┌─────────────────────────────────┐
                              │ Guest daemon (`guestd`)         │
                              │  • workspace and pod lifecycle  │
                              │  • process supervision          │
                              │  • images, files, chats, Git    │
                              │  • guest-side network capture   │
                              │                                 │
                              │  ┌───────────┐  ┌───────────┐   │
                              │  │ Pod A     │  │ Pod B     │   │
                              │  │ podd PID 1│  │ podd PID 1│   │
                              │  │ processes │  │ processes │   │
                              │  └───────────┘  └───────────┘   │
                              └─────────────────────────────────┘
```

The four runtime components have deliberately different responsibilities:

| Component | Location | Primary Responsibility |
| --- | --- | --- |
| Host daemon (`hostd`) | Host | Owns workspace VMs and mediates Tascarrel access to host resources |
| Guest daemon (`guestd`) | Workspace VM | Owns images, pods, task processes, and workspace services |
| Pod init (`podd`) | Pod, as PID 1 | Bootstraps the pod, reaps orphaned processes, and optionally supervises `dockerd` |
| Pod client (`podctl`) | Pod, when invoked | Calls Tascarrel as the authenticated current pod |

The installed `tascarrel` executable contains the host daemon; `hostd` is the
short name used here and in the source for that host-side subsystem. The other
long-form executable names are `tascarrel-guestd` and `tascarrel-podd`.

Host-side clients communicate only with `hostd`. It owns QEMU, the virtual
machine monitor, and mediates access to host resources. Requests concerning a
workspace or pod are routed to `guestd` in the relevant virtual machine.

`guestd` directly launches and supervises task processes inside pods through
the `runc` container runtime. `podd` is not an intermediate process-control
daemon: it performs pod-local bootstrap, acts as PID 1, supervises the optional
per-pod Docker daemon, and helps `guestd` establish pod-local connections.

The only alternate route is for `podctl`. It runs inside a pod and connects to
an authenticated, pod-private listener served by `guestd`. `podd` creates the
listener inside the pod namespace but does not handle its steady-state control
or Git traffic.

## Security Model

> [!WARNING]
> **Tascarrel is under active development.** Things may break, and its security
> properties have not been independently audited.

See the [security policy](SECURITY.md) for vulnerability reporting.

There are two different isolation claims:

- The workspace VM is intended to protect the host and other workspaces.
- A pod is intended to protect privileged guest services and give each task a
  clean environment. Pods in one workspace do not form a security boundary
  from one another.

### Workspace Boundary

Each workspace runs in a dedicated QEMU virtual machine. The virtual machine is
the primary boundary between a workspace, the physical host, and other
workspaces.

A compromised workspace must not access the host or another workspace except
through resources deliberately exposed by the host. Crossing this boundary
otherwise requires a vulnerability in the host kernel, hypervisor, device
emulation, host daemon, or another trusted host component.

The host owns workspace lifecycle, configuration input, upstream repository
access, secret providers, external network access, published ports, and device
attachment. Managed virtual machines have a private control device but no
conventional network interface.

### Pod-to-Guest Boundary

Pods run untrusted development workloads inside a workspace virtual machine.
Each pod has Linux user, mount, PID, network, IPC, UTS, and control-group
(cgroup) namespaces, a private mount tree, seccomp and AppArmor policy, and a
device allowlist. Together, these mechanisms separate the pod from the
privileged operating system around it, called the **guest**. Root and Linux
capabilities inside a pod are scoped to its user namespace and do not grant
root privileges in the guest.

A compromised pod must not gain control of `guestd` or access unexposed guest
resources without a vulnerability in the guest kernel, `runc`, the Tascarrel
runtime, or an enabled privileged guest service. This boundary protects every
pod and all persistent state in the workspace.

Optional features deliberately expand a pod's interface to the guest. Docker
and Podman enable container-building facilities, virtualization exposes KVM,
the Nix daemon exposes a workspace service and store, and USB attachment
exposes selected device nodes. Their runtime policies remain inside the
virtual-machine boundary, but each feature increases what a compromised pod can
reach within its workspace.

### Pods Within a Workspace

Pods in the same workspace are not strong security boundaries from each other.
They share the guest kernel and may share explicitly configured caches, the Nix
store and daemon, editor state, attached devices, and other workspace-wide
services. A compromised pod may affect another pod by poisoning or monopolizing
those resources.

Pods provide disposable task environments and process, filesystem, cgroup, and
network isolation. Workloads requiring separation from mutually hostile code
belong in different workspaces.

### Mediated Access

Workspace access to the host and external systems is mediated rather than
provided directly. The host daemon opens external connections, applies
host-owned network policy, manages upstream Git credentials, resolves secrets,
publishes selected ports, and attaches selected devices. Guest and pod
identities are preserved across these paths so policy can be applied to the
originating workload.

Enabling a cache, published port, host-port mapping, network-policy exception,
secret exposure, or device attachment grants a specific additional interface.
These grants must not expose unrelated host resources or bypass the
virtual-machine boundary.

### Trusted Inputs and Components

The Tascarrel installation, host configuration, and workspace definition are
trusted inputs. Workspace definitions include image build instructions, build
contexts, setup and init scripts, overlays, agent configuration, and Nix
expressions. A malicious definition may compromise its own workspace virtual
machine; the virtual-machine boundary must still protect the host and other
workspaces.

The workspace boundary trusts the host kernel, QEMU, device emulation, the host
daemon, and other host-side services used by Tascarrel. The pod-to-guest
boundary trusts the guest kernel, `guestd`, `runc`, `podd`, and enabled
privileged guest services.

Image building and the optional workspace-wide Nix daemon run as privileged
guest services. They are trusted within their workspace and must remain
inaccessible to untrusted pod workloads except through their intended
interfaces.

### Accepted Risks and Non-Goals

The following do not, by themselves, violate the security model:

- One pod affects another through deliberately shared workspace state.
- A pod exhausts resources or otherwise denies service to its workspace or the
  host. Availability is out of scope.
- A pod reads, modifies, corrupts, or attacks a resource or device explicitly
  exposed to its workspace, subject to its configured access mode.
- A vulnerability in a trusted kernel, hypervisor, runtime, or privileged
  service defeats the boundary that depends on it.

Unexpected access across a stated boundary, or bypassing configured secret,
filesystem, device, or network policy, is a security defect.

## Implementation

### Host Control Plane

The **control plane** is Tascarrel's management API. Actions request a change or
a point-in-time result; subscriptions stream live state changes. Both use
schema-defined messages so the browser, CLI, `hostd`, and `guestd` agree on the
shape of every operation.

The `tascarrel` daemon is the host-side entry point. It serves the browser UI
and streaming HTTP endpoints, carries browser control messages over a
WebSocket, and accepts CLI control connections on a private Unix socket. The UI
and CLI therefore use the same control plane over different transports.

Host-owned services manage workspace configuration, QEMU lifecycle, repository
caches and approvals, secret providers, external networking, published
services, and USB forwarding. A workspace virtual machine starts lazily when an
operation needs it. The host owns QEMU and any filesystem-sharing helper
processes, and stops and reaps them when the workspace or daemon stops.

Each managed virtual machine boots directly into a small NixOS system from an
immutable system image. Its persistent state disk is sparse, so host storage is
allocated as data is written, while its root filesystem is recreated on every
boot. A private virtual serial device acts as the VM's control link to `hostd`.

### Workspace Transport and Routing

Each VM has one private host connection. A **multiplexer** divides that
connection into independent logical channels, allowing unrelated services to
share it without mixing their messages. Those channels carry:

- The bidirectional typed control plane.
- Attributed DNS and TCP relays.
- Workspace configuration and environment snapshots.
- Repository and Git smart-protocol traffic.
- File and chat-attachment streams.
- Connections from published host services to pod loopback ports.

The control plane addresses operations to the host, a workspace, or a pod.
`hostd` executes host-owned operations locally and forwards workspace and pod
operations to the corresponding `guestd`. `guestd` implements both workspace
and pod operations locally; when it or `podctl` invokes a host-owned operation,
it forwards that operation back over the same full-duplex connection.

Large payloads use dedicated multiplexed or HTTP streams instead of being
embedded in control messages. Every admitted request is associated with the
identity of the client, workspace, or pod that initiated it. The first daemon
assigns an identity when necessary, and each later hop validates it before
forwarding the request. A caller therefore cannot claim to be another
workspace or pod.

### Guest Control Plane and Process Supervision

Each virtual machine runs `tascarrel-guestd` as its privileged workspace
supervisor. Its services own image builds, pod lifecycle, process state and
I/O, terminals, files, repository changes, chats and coding-agent
integrations, the browser-based Code editor, guest metrics, and guest-side
network capture.

`guestd` is also the process supervisor for every pod. It validates a process
request, optionally starts the pod, creates a standard Open Container Initiative
(OCI) process description, and launches it with the `runc` runtime. `guestd`
then retains the process controls, terminal or pipe I/O, logs, and observable
status until the process exits or is removed. Configured init steps, terminals,
coding agents, setup commands, and other managed services all use this
supervision path.

Processes are associated with a pod, but their control state lives in
`guestd`. Stopping a pod terminates its processes by destroying the `runc`
container; restarting `guestd` reconciles leftover transient runtime state to
stopped durable pod records.

### Pod Runtime and `podd`

`guestd` uses `runc` to prepare each pod from its stored filesystem. Before
starting the pod, it establishes the Linux namespaces, user and group mappings,
mounts, cgroup, security policy, devices, and network. Each pod receives a
distinct range of guest user and group IDs, so root inside the pod maps to an
unprivileged identity outside it.

`tascarrel-podd` then starts as PID 1 inside the pod. Its responsibilities are
limited to pod-local init work:

- Prepare delegated cgroups and rootless-container runtime directories when
  the corresponding features are enabled.
- Run the pod-local `hooks/init` scripts and record their health.
- Start and supervise `dockerd` when managed Docker is enabled.
- Reap orphaned children and terminate the PID namespace during shutdown.
- Complete an authenticated readiness handshake with `guestd`.
- Provision the pod-private `guestd` listener and relay requested connections
  to pod loopback ports.

Task processes are not launched or managed through `podd`. `guestd` starts and
observes them directly through `runc exec`; `podd` receives no Tascarrel
control-plane operations.

### Storage and Images

The VM's persistent state disk uses Btrfs, a copy-on-write filesystem that can
create cheap snapshots. It holds durable guest metadata, built environments,
repository checkout generations, chat and coding-agent state, shared caches,
and per-pod storage. Runtime sockets, network namespaces, and `runc` bookkeeping
are temporary and are recreated after a VM restart.

A workspace image is the prepared operating environment from which pods start.
The host transfers its Dockerfile and build context into the VM instead of
mounting the host directory. `guestd` builds the image with a private instance
of BuildKit, a container-image builder, validates the resulting standard
container image, and publishes its root filesystem as an immutable Btrfs
snapshot. Setup steps run in a temporary pod; after they succeed, Tascarrel also
freezes the prepared workspace state for use by later pods.

Each pod pins one immutable image version and receives separate writable
filesystem trees, represented as Btrfs subvolumes, for its root filesystem,
workspace, Docker data, and temporary data. Explicitly configured caches are
separate workspace-level subvolumes mounted into every pod in that workspace.
Destroying a pod removes its private subvolumes without modifying the image or
other pods.

Pod, image, and chat records live in a SQLite database in the guest. On
startup, `guestd` compares those durable records with Btrfs and any leftover
`runc` state. Previously running pods become stopped records instead of being
assumed to contain live processes.

### Networking, Ports, and Secrets

Managed workspace VMs have no general-purpose network interface. The two
directions of communication are handled explicitly.

For outbound traffic, the guest firewall redirects DNS queries and TCP
connections from pods, image builds, and selected workspace services to
`guestd`. It records which workload originated the traffic, then sends the DNS
question or requested TCP destination to `hostd`. `hostd` applies the
workspace's hostname, address, port, and local-network policy before resolving
the name or opening an external socket.

On configured HTTPS ports, `hostd` admits connections using the normalized TLS
SNI and resolves that server name itself before relaying TLS unchanged. It
terminates TLS only when an HTTPS secret-injection rule matches the SNI. In that
case, every forwarded HTTP host must match the SNI. Each secret-injection rule
also lists the HTTP methods it admits. When a host matches one or more rules,
`hostd` rejects the request unless at least one matching rule admits its method.

The guest firewall also denies direct pod-to-guest and pod-to-pod traffic unless
Tascarrel provides an explicit path. A pod reaches configured services on the
host through a reserved synthetic address, which `hostd` translates only to
approved ports bound to the host's loopback interface (`localhost`).

For inbound traffic, `hostd` owns the loopback listeners and HTTP routes for
published pod services. Once a connection is accepted, it asks `guestd` to
connect the stream to the selected pod's loopback port. `podd` performs only
this last in-namespace connection handoff.

Secret providers and encrypted secret documents also remain host-owned. A
secret becomes visible to a workspace only through an explicit operation:

- Secret references in workspace environment values are resolved by `hostd`;
  the resulting plaintext environment is sent to `guestd` and inherited by
  pod processes.
- HTTP secret injection is performed by the host proxy for matching requests,
  after the request has left the pod; each rule restricts the admitted HTTP
  methods, and the value is not placed in the pod's environment or filesystem
  by Tascarrel.
- Host-facing secret management actions may reveal or mutate values for an
  authorized client.

These paths expose only the configured value. They do not grant a workspace
access to the provider or to unrelated secrets.

### Repository Mediation

`hostd` is the only component that contacts upstream Git servers. It maintains
private bare repositories as object caches, uses host credentials for fetches
and pushes, and publishes specific repository versions to `guestd`. New pods
receive writable workspace snapshots prepared from those versions; they do not
receive host filesystem mounts or upstream credentials.

Pod Git traffic uses the authenticated pod-private multiplexer. Fetches, pulls,
and rebases are served from Tascarrel's prepared repository state. A push
travels through `guestd` to `hostd`, where the proposed branch and tag updates
are staged for explicit approval. When approved, `hostd` checks that the
upstream repository has not changed unexpectedly before publishing them.
