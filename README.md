<p align="center">
    <img src="frontend/src/assets/tascarrel.svg" width="8%" alt="Tascarrel Logo">
</p>
<h1 align="center">
    Tascarrel
</h1>
<h4 align="center">
    Local, Isolated Development Environments for You and Your Agents
</h4>

Tascarrel is a **secure development environment** for humans and their coding
agents, built around two ideas:

- **Isolation by default.** Each workspace runs in a dedicated VM; lightweight,
  disposable pods give every task within a workspace a clean environment without
  requiring a dedicated VM for each. Host-mediated interfaces make access to
  networking, secrets, Git, ports, and hardware explicit and configurable.
- **Oversight without babysitting.** Agents can keep working in the background
  while Tascarrel preserves their sessions, surfaces their progress, and calls for
  attention when human judgment is needed.

> [!CAUTION]
> **Tascarrel is highly experimental.** It may not work as expected and may change
> or break without notice. **Do not rely on it as a production security
> boundary.** Use it only for testing and development purposes and with data you
> can afford to lose or leak. Use at your own risk.

[**Get started with Tascarrel in a few minutes.**](#installation) 🚀

## Workspaces and Pods

A **workspace** groups the projects, tools, and access needed for a development
context. You might create separate workspaces for personal and work projects,
one for each customer, or a product workspace spanning frontend, backend, and
infrastructure repositories. Its resources and settings persist across tasks.
Each workspace runs in its own VM, securely separating it from the host and
other workspaces.

A **pod** is an isolated, disposable environment for one task. It starts from
the workspace's projects, tools, and settings and has its own filesystem state,
processes, and network. Create one for an agent fixing a bug, a new feature, or
an interactive exploration. Several pods can run in parallel and share resources
such as caches when configured for the workspace. Pods within the same workspace
have separate process, filesystem, cgroup, and network namespaces, but they
share the VM's kernel and are not security boundaries from one another.

## Highlights

Tascarrel focuses on practical development workflows:

- **Start clean, repeat quickly.** New pods automatically start with the
  workspace's current tools and repository state. Configured caches make
  repeated work faster.
- **Run agents without babysitting.** Agents work independently in isolated
  pods, while Tascarrel calls for your attention when review or input is needed.
- **Understand and ship changes.** Browse files, inspect diffs and agent
  activity, then capture or push commits through host-controlled Git.
- **Keep request credentials on the host.** HTTP-injected secret values remain
  in `hostd` and are added only to requests for approved HTTP or HTTPS
  destinations. Environment interpolation remains available when a process
  explicitly needs the plaintext value.
- **Control outbound access.** Restrict pod egress by hostname, address, and
  port.
- **Run full development workloads.** Use rootless Podman or nested VMs, and opt
  into managed Docker and Nix daemons.
- **Connect real hardware.** Attach declared USB devices for embedded
  development, robotics, hardware security, and physical AI.

## Why Tascarrel?

Developing directly on your main machine is a liability, especially when coding
agents are involved. Dependencies, build scripts, tools, and agent actions run
beside your source and credentials, often with access to private networks and
cloud resources. A bug, malicious package, or misguided agent action can affect
far more than the task at hand. Development should happen in isolation, with
host and network access granted deliberately.

Isolation is not new; choosing the boundary is the hard part. Cloud workspaces
separate work from the laptop, but can sacrifice locality, offline use, and
hardware access. A VM for every task provides a separate kernel, but costs more
to start and run. Containers are lightweight, but share the host kernel and are
often given host mounts, daemon sockets, credentials, and broad network access.
Each choice balances isolation, locality, and cost differently.

Tascarrel takes a layered approach: one local VM per workspace and lightweight,
copy-on-write pods per task. The VM separates each workspace from the host; pods
make clean, parallel environments practical without a VM per prompt.
Host-mediated interfaces for networking, secrets, Git, ports, and hardware make
those crossings explicit and configurable.

Isolation is only half the problem. Software engineering is not yet a dark
factory that can run without people. Agents can close some routine,
well-specified tickets, but novel work still calls for human judgment. Good
tooling should support oversight without demanding babysitting. It should let
agents work in the background, make their status and output easy to grasp, and
call for attention when a decision, failure, or review needs a human.

Tascarrel is an experiment in local-first development for people and agents.

## Architecture

See [ARCHITECTURE.md](ARCHITECTURE.md) for the host/guest design, isolation
boundaries, pod runtime, storage model, networking, and source layout.

## Installation

Tascarrel is distributed as one architecture-specific executable. It contains
the CLI, host daemon, and an xz-compressed payload archive with the read-only
guest system image, Linux kernel, initrd, and browser UI. QEMU and hardware
virtualization support are host dependencies; no host boot firmware is
required. Tascarrel also requires Git and a Chromium-family browser. SOPS is
optional and needed only for SOPS-backed secrets.

The initial release targets:

- Apple Silicon macOS with QEMU's Hypervisor Framework acceleration and
  LaunchAgents.
- x86-64 and AArch64 Linux with KVM access and systemd user services. The
  statically linked Linux executable runs directly on NixOS as well as
  conventional Linux distributions.

Install the host dependencies first, then run the same installer on macOS or
Linux:

```console
curl --proto '=https' --tlsv1.2 -fsSL \
  https://tascarrel.dev/install.sh | sh
```

The installer detects the host system and architecture, downloads the matching
archive and checksum from the latest GitHub release, verifies it, installs
`tascarrel` at `~/.local/bin/tascarrel`, and runs `tascarrel install`. That final
command checks the host and registers a LaunchAgent on macOS or a systemd user
service on Linux.

Start the persistent service with:

```console
~/.local/bin/tascarrel daemon start
```

Alternatively, run `~/.local/bin/tascarrel app` for a foreground session that
opens its own app window and stops the daemon when that window closes.

### Runtime Behavior

The installer uses `$HOME/.tascarrel` unless `TASCARREL_HOME` is already set.
Configuration lives below `$TASCARREL_HOME/config`; persistent and transient
state lives below `$TASCARREL_HOME/state`. When the binary is run without the
installer, `TASCARREL_HOME` defaults to `.tascarrel` in the current directory.
Every host start verifies and extracts the embedded assets under
`$TASCARREL_HOME/state/payloads/<sha256>/`, then removes older payload generations.

App mode runs the daemon inside the Tascarrel process and opens the UI in a
dedicated browser window. Closing the window stops the daemon and its workspace
VMs. `TASCARREL_APP_BROWSER` can select a specific browser executable. App mode
does not install the binary or register a service, and an installed daemon must
be stopped with `tascarrel daemon stop` before app mode starts.

`tascarrel install` repeats the dependency checks, installs the executable, and
registers the platform's user service. Use `tascarrel daemon status`, `restart`,
`stop`, and `logs` to manage it.

## User Interface and Agents

The UI is Tascarrel's primary product surface. While the daemon is running, it is
available at `http://127.0.0.1:8272` by default; app mode opens its private
address automatically. The UI files are extracted with the embedded payload
and served directly by hostd. Each pod has persistent terminal and Codex tabs,
a workspace file browser, and a repository review surface backed by
`@pierre/diffs`. Review compares local branches with an upstream base, shows
commits and uncommitted file changes, and publishes selected branches through
host-mediated Git. Starting an isolated task from the left sidebar creates both
the pod and its first Codex thread. Each chat exposes the models and
model-specific reasoning efforts advertised by Codex, remembers the selection
across reloads, and applies it to subsequent turns. Non-ephemeral Codex rollouts
live in the pod's persistent home and are explicitly resumed when a replacement
app-server starts, including after a Tascarrel daemon or workspace VM restart.
While a turn runs, messages can either steer that same Codex turn or be
scheduled to start after it. Tascarrel owns and durably stores the ordered
schedule so it keeps advancing when the browser is closed; scheduled messages
can be edited, reordered, or removed in the chat UI. Attention badges report
pending approvals, failed turns, and unread completions.

The guest image pins the official Codex binary and guestd mounts that immutable
version read-only into every pod. Tascarrel does not copy the host's Codex
configuration into a pod. Harness authentication is guest-managed and
workspace-wide: guestd stores provider state in protected workspace storage and
mounts it into each pod under `/opt/tascarrel/chat`. It also installs the
workspace-wide `agents/AGENTS.md` as Codex's global guidance. Workspace skills
are mounted read-only at `~/.agents/skills`. The agent transport includes an
explicit harness name so additional agent harnesses can be added without
changing the terminal, file, or repository APIs.

Codex threads use `approvalPolicy = "never"` and `dangerFullAccess`: the pod is
the sandbox, so the agent can use every tool available inside it without pausing
for command or file approvals. This does not grant access outside the pod's
existing Tascarrel isolation and egress policy.

## Workspace Configuration

A named workspace takes its configuration inputs from:

```text
$TASCARREL_HOME/config/workspaces/<name>/config.toml
$TASCARREL_HOME/config/workspaces/<name>/settings.json
$TASCARREL_HOME/config/workspaces/<name>/image/Dockerfile
$TASCARREL_HOME/config/workspaces/<name>/agents/AGENTS.md
$TASCARREL_HOME/config/workspaces/<name>/agents/skills/<skill>/SKILL.md
```

`config.toml` is decoded using the generated configuration schema and may be at
most 4 MiB. Unknown fields are rejected so configuration spelling remains
consistent across the host and guest daemons.

The optional `settings.json` stores portable interface preferences and may be
committed with the workspace configuration. Hostd reads and writes this file;
it is not transferred into the VM. Chat model preferences are keyed by harness
and use provider-native model identifiers:

```json
{
  "chat": {
    "harnesses": {
      "claudeCode": {
        "defaultModel": { "model": "claude-sonnet-5", "options": [] },
        "modelOrder": ["claude-opus-4-8", "claude-sonnet-5"],
        "hiddenModels": [],
        "favoriteModels": ["claude-opus-4-8"]
      }
    }
  }
}
```

The other harness key is `codex`. Credentials and chat state are not stored in
this file.

Workspace slash commands are harness-independent message snippets defined in
`config.toml`. Typing `/` at the start of a composer opens the command menu.
Enter sends the selected expansion immediately, while Shift+Enter or Tab inserts
it for editing:

```toml
[chat.commands.review]
text = """
Review the current changes. Focus on correctness, regressions, security issues,
and missing tests. Report findings in order of severity.
"""
```

## Workspace Lifecycle

The minimal CLI can create a default Debian workspace with Zsh, Starship, and
the managed Docker daemon enabled, then manage its VM lifecycle:

```console
tascarrel workspace create demo
tascarrel workspace list
tascarrel workspace start demo
tascarrel workspace info demo
tascarrel workspace stop demo
```

`start` starts the workspace VM and waits for guest readiness. `stop` preserves
its configuration, pods, disks, and retained repository state. `info` reports
the current VM lifecycle or startup failure. Runtime logs and feature-specific
diagnostics are available in the UI.

To stop a workspace VM and permanently remove its configuration and state:

```console
tascarrel workspace delete demo
# Non-interactive use requires explicit confirmation:
tascarrel workspace delete demo --force
```

Lifecycle commands require the per-user daemon to be running. Deletion removes
the workspace configuration last, after VM shutdown and state cleanup complete.

## Workspace Runtime

Hostd streams the workspace configuration, Docker build context, and configured
overlay to the guest over `tascarrel-control`; it does not mount those workspace
directories into the VM.
The first pod refreshes the host input and builds an image when no available
generation has the same authoritative SHA-256. Later pod creation reuses that
generation, including across workspace VM restarts, and automatically builds
again when the input digest changes. The Images page shows the retained image
inventory, BuildKit and setup output, and an explicit build action. That action
provides a refresh boundary that rebuilds even when the input digest is
unchanged, which is useful for mutable base-image tags.

Tascarrel runs user-facing processes as a non-root image user. When the built
image resolves to root, Tascarrel uses the account with UID 1000 regardless of
its name. If UID 1000 is unused, it reuses an existing `develop` account or
creates `develop` with UID 1000, then prepares that account's home directory.
An explicit non-root Dockerfile `USER` retains its UID, primary GID, and name;
`USER root`, `USER 0`, and an omitted `USER` all receive the same non-root
normalization. When an image already contains a `docker` group, Tascarrel also
adds the effective image user to that supplementary group without creating the
group itself. `/etc/subuid` and `/etc/subgid` are normalized to delegate IDs
65536 through 131071 to that user, matching the secondary half of every pod's
outer ID map.

VM sizing is automatic: each workspace VM receives every CPU available to
the Tascarrel daemon and one third of the host memory available when the daemon
starts. A workspace can override either value independently:

```toml
[vm]
cores = 8
memory = "16G"
disk = "1T"
```

Memory and disk sizes use binary `M`, `G`, or `T` units; explicit forms such as
`MiB` and `GiB` are accepted too. The disk is a sparse persistent state disk:
its host allocation grows with writes and trimming releases deleted blocks.
Increasing `disk` grows the image and its Btrfs filesystem on the next VM
start; decreasing it does not shrink existing storage.

Optional runtime facilities are workspace-wide and derived from the enabled
features. Their defaults are equivalent to:

```toml
[features]
docker = false
podman = false
virtualization = false
usb = false

[nix]
daemon = false
```

The Code tab asks guestd to run code-server as a supervised pod process and
loads it through an internal hostd HTTP route. It installs the extension
required by Tascarrel's default theme automatically. Additional Marketplace
extensions can be installed before code-server starts:

```toml
[editors.code]
extensions = [
  "jnoortheen.nix-ide",
  "rust-lang.rust-analyzer",
]
```

Settings and extensions live in one persistent workspace profile shared by
every pod at `~/.tascarrel/editors/code/profile`, so switching pods retains
user changes. Optional `settings.json` and `keyboardLayout.json` files from
`~/.tascarrel/editors/code/config` seed their matching files in the profile
without replacing existing profile configuration.

Persistent caches shared by every pod are optional:

```toml
[[caches]]
name = "cargo-cache"
path = "~/.cache/cargo"
```

Repositories are declared by their destination below `/workspace`:

```toml
[repos."src/tascarrel"]
source = "ssh://git@example.org/team/tascarrel.git"
```

Git pushes require approval by default. A workspace policy can automatically
publish selected branches while retaining protected branches and all tags for
approval:

```toml
[git]
default-policy = "allow"

[[git.branches]]
pattern = "main"
policy = "require-approval"

[[git.branches]]
pattern = "release/**"
policy = "require-approval"

[[git.tags]]
pattern = "**"
policy = "require-approval"
```

Rules match short branch or tag names in declaration order. `*` matches within
one slash-delimited component and `**` matches across components. The supported
policies are `allow`, `deny`, and `require-approval`. A repository can replace
the complete workspace policy:

```toml
[repos."src/tascarrel".git]
default-policy = "deny"

[[repos."src/tascarrel".git.branches]]
pattern = "automation/**"
policy = "allow"
```

The most permissive workspace policy is `[git]` with
`default-policy = "allow"` and no rules. Ref deletion remains unsupported.

Workspace-wide pod environment and files layered into every new workspace seed
are optional:

```toml
[[setup.steps]]
script = '''
git config --global init.defaultBranch main
printf 'prepared\n' > /workspace/.prepared
'''

[[init.steps]]
script = '''
printf 'started\n' > /workspace/.pod-started
'''
# Optional; false by default. This step otherwise remains supervised asynchronously.
wait = true

[env]
EDITOR = "vim"
PROJECT_MODE = "development"
# Environment interpolation deliberately makes the plaintext value available
# to processes in the workspace.
OPTIONAL_PROCESS_TOKEN = "${secrets.project.PROCESS_TOKEN}"

[network]
# Make these host-loopback TCP services available at host.tascarrel.internal.
# An integer keeps the same port; a string maps <host-port>:<pod-port>.
host-ports = [3000, "5432:15432"]

[secrets.providers.project]
kind = "sops"
# Relative to this workspace directory; defaults to secrets.json.
file = "secrets.json"

[[network.secret-injection]]
host = "api.example.com"
header = "authorization"
# Optional; defaults to tascarrel-secret:api-token for project.API_TOKEN.
placeholder = "replace-with-api-token"
secret = "project.API_TOKEN"

[features]
usb = true
```

The optional conventional `overlay/`, `hooks/setup/`, `hooks/init/`, and
`agents/` directories are transferred to the guest in the same bounded
workspace snapshot. `overlay/` is validated as a bounded tree
and copied into `/workspace` while running as the pinned image user, so Tascarrel
does not repair ownership with a later `chown` walk. Workspace environment
values override Dockerfile `ENV` values with the same name; runtime-owned
identity and service variables still take precedence.

`agents/AGENTS.md` supplies workspace-wide guidance to every Codex session,
before any repository or nested `AGENTS.md` files Codex discovers. Skills below
`agents/skills/<name>/SKILL.md` use Codex's normal user-scope discovery and may
include their usual references, scripts, assets, and metadata. Both trees are
read-only inside a pod and pinned to the workspace input generation used when
that pod was created. Pod creation refreshes this runtime input without
rebuilding the prepared image; existing pods retain their original generation.
The common source is also available to future harness adapters at
`/run/tascarrel/agents`.

An optional `.env` file beside `config.toml` is transferred as part of the same
bounded snapshot and re-read from a freshly fetched snapshot whenever a process
starts. Its values override image and `[env]` values; environment entries on a
typed process request override `.env` for that process.

## Networking and Hardware

`[network].host-ports` exposes selected services bound to host loopback. Entries
accept `<port>` as same-port shorthand or a quoted
`"<host-port>:<pod-port>"` mapping. Every pod resolves
`host.tascarrel.internal` to a reserved synthetic address; its traffic is
intercepted in the guest and carried to hostd, which connects to the mapped
`127.0.0.1:<host-port>`. Other ports at that address are denied, and the
synthetic route does not make arbitrary host-local addresses reachable.
Pod-scoped mappings can also be added and removed while a workspace is running
from the Network tab. A dynamic mapping overrides the configured mapping for
the same pod-visible port and is removed when the workspace is destroyed.

Secret providers are host-owned. The SOPS provider reads an encrypted,
string-valued JSON document with the host's credentials; its values can be
managed from the workspace Secrets settings. An HTTP injection rule matches an
exact host or a `*.example.com` subdomain pattern. `header` limits replacement
to one request header and should normally be set. Without it, Tascarrel checks
every eligible non-routing header. Basic Authorization values are decoded,
replaced, and encoded again.

HTTP and HTTPS inspection defaults to ports 80 and 443. Use `http-ports` or
`https-ports` to replace those defaults. Hostd validates the HTTP `Host`, checks
it against TLS SNI for HTTPS, resolves the destination itself, applies address
policy, and then replaces the configured placeholder. The per-workspace CA is
added to common certificate bundles in each pod so curl, Git, Python, Node, and
OpenSSL-based clients can validate intercepted HTTPS. Injection and hostname
policy currently support HTTP/1.1, including protocol upgrades, but not HTTP/2,
QUIC, or `CONNECT` tunnels.

Changing network rules, provider configuration, environment references, or CA
enablement requires restarting the workspace VM. Updating a value in an
existing provider takes effect on the next matching HTTP request. Environment
references are resolved during workspace startup and enter the process
environment by design.

The reverse direction is dynamic: inside a running pod, use
`podctl ports publish <port>` to publish a pod service and print the dynamically
assigned host-loopback port. Add `--title <name>` to label it in the fixed web UI
Ports tab and `--tab` to create a visible HTTP route as well. `podctl ports
list` shows only this pod's forwards, and `podctl ports unpublish <port>` closes
its host listener. `podctl http publish <port>`, `list`, and `unpublish` manage
HTTP routes independently.

The `podctl` command is a standalone client. Its control-plane RPCs,
subscriptions, and Git smart-protocol streams share one authenticated,
pod-private guestd socket and are separated by the Tascarrel multiplexer. The
listener itself assigns the workspace and pod identity; the client cannot
select either. Podd is involved
only in provisioning the listener inside the pod namespace, not in steady-state
request routing. `podctl processes` and `podctl chats` expose pod-filtered
inspection commands over the same connection.

Pod names are stable machine identifiers. Inside the pod, `podctl title
<title>` sets its separate human-readable task title in the UI.
`podctl destroy` destroys the current pod and all of its persistent resources.

USB forwarding is currently Linux-host-only and requires `[features] usb = true`.
For a selected running workspace, the USB button in the left sidebar shows the
live host inventory and whether a device lacks usbfs access or is already
forwarded to another workspace. Attaching never starts a stopped workspace, and
one physical device can be forwarded to only one workspace at a time.

An attachment exists only for the current VM runtime. Detaching, stopping the
workspace, or physically unplugging the device removes it. Reconnecting a
physical device requires another explicit attach. Guestd exposes both
kernel-derived nodes such as `/dev/ttyACM0`, `/dev/hidrawN`, or `/dev/videoN` and
the raw `/dev/bus/usb/...` node to every current and future pod in the workspace.
Visibility is workspace-wide, but hardware and drivers may still permit only one
effective user at a time—especially serial, raw USB, and block devices.

## Pod Lifecycle and Isolation

An image build refreshes the image, repositories, and conventional directories,
starts a temporary pod, and runs every `[[setup.steps]]` script in declaration
order, followed by every regular file in `hooks/setup/` in lexical order,
through a container-root shell in `/workspace`. Setup steps are always
synchronous because Tascarrel can publish the immutable rootfs/workspace snapshot
pair only after every step succeeds. New pods are writable Btrfs snapshots of
that pair. Refreshing repositories or the overlay invalidates the prepared pair
until setup succeeds again.

Whenever podd starts a pod, it starts each `[[init.steps]]` script in declaration
order as the image user, with its supplementary groups and `/workspace` as the
working directory. Steps are asynchronous by default. `wait = true` makes that
specific step finish before startup proceeds. A failed readiness-blocking step
marks the pod failed and releases its runtime resources; a failed asynchronous
step is recorded without blocking readiness. Regular non-hidden files in
`hooks/init/` run afterward as one asynchronous, lexically ordered group; their
failures mark the pod degraded without stopping it. Each pod receives a read-only
mount pinned to the hook generation that existed when it was created.

The managed Docker daemon also starts asynchronously. Pod readiness does not
imply that `/run/docker.sock` is ready yet, but podd continues supervising
dockerd and stops the pod if the daemon exits.

## Optional Runtime Features

Enabling Podman with `[features] podman = true` injects the Nix-provided Podman
executable at `/usr/local/bin/podman` and grants the image user the
rootless-container facilities Podman needs: `CAP_SETUID`, `CAP_SETGID`,
subordinate IDs, a delegated cgroup subtree, `/dev/fuse`, and `/dev/net/tun`.
Tascarrel also injects the Nix-provided `newuidmap` and `newgidmap` helpers
read-only. The image does not need to install or configure Podman itself.

Enabling Docker with `[features] docker = true` starts and supervises a confined
dockerd in every pod, injects `/usr/local/bin/docker`, and grants the pod's outer
user namespace the broader mount, device, cgroup, and namespace operations
Docker needs. The daemon socket is assigned directly to the image user's numeric
primary GID, so the Dockerfile does not need to create a `docker` group or add
the user to it.

Enabling virtualization with `[features] virtualization = true` exposes
`/dev/kvm` in every pod. It is disabled by default. Under Linux KVM, Tascarrel
already passes the host CPU through to the workspace VM; when the host exposes
its virtualization extensions, programs in the pod can therefore launch
hardware-accelerated virtual machines.

Every pod uses seccomp, AppArmor, a private mount tree, and a device allowlist.
Standard and virtualization-enabled pods also mask sensitive procfs paths.
Rootless Podman cannot use child masking mounts beneath the outer procfs/sysfs
tree because Linux must permit fresh mounts from its nested user namespace; its
outer process retains only the two set-ID capabilities, while namespace
ownership and AppArmor rules continue to protect sensitive guest interfaces.
Docker and Podman permit the container-assembly operations they require.
Virtualization adds only `/dev/kvm` and does not relax the syscall or filesystem
policy. Runtime capabilities cannot be configured independently of these
features.

Setting `daemon = true` under `[nix]` gives pods a workspace-wide Nix daemon
that is separate from the VM operating system's daemon. Its store is a Btrfs
subvolume on the persistent state disk, so downloaded and built paths are
shared across pods and survive pod destruction, VM restarts, and immutable
system image upgrades.
Pods see the store read-only at `/nix/store` and reach the daemon through the
standard `/nix/var/nix/daemon-socket/socket` path.

Tascarrel enables `nix-command` and `flakes` in the VM and pod clients, and sets
`NIX_REMOTE`, `NIX_STATE_HOME`, and `NIX_PROFILE` for shell and exec processes.
`TASCARREL_NIX_GCROOTS` names an additional directory for explicit roots. Both
state locations live below the pod's private direct-root subtree at
`/nix/var/nix/gcroots/tascarrel/pods/<pod-id>`; no other pod receives that mount.
The subtree survives guestd and VM restarts and is withdrawn atomically only
after the pod has stopped during destruction. Tascarrel injects the compatible
`nix` client at `/usr/local/bin/nix` without exposing the package's other
executables.

The pod Nix daemon starts pressure collection at 512 MiB free space and targets
1 GiB. A persistent weekly timer, randomized by up to six hours, also reclaims
up to 4 GiB of unreachable paths without deleting old profile generations.
The direct socket preserves the pod's mapped peer UID, so it remains an
untrusted Nix client rather than acquiring VM-root trust. Nix builds currently
perform egress as the VM daemon, however, so their traffic is not attributed to
the requesting pod/user/cgroup. Filtering and per-request build attribution
are future work.

## Repository Mediation

The host daemon keeps a private bare object cache for each configured source in
each workspace and is the only component that talks to upstreams or uses host
credentials. Typed image and pod operations prepare exact cache versions and
atomically publish them as the seed for future pod workspaces. Each pod receives
a writable Btrfs snapshot of that seed; its configured `origin` uses the
pod-private control socket to request the configured snapshot through guestd,
so pulls and rebases need no direct upstream access. Repository preparation
rereads workspace configuration while the VM is running. Publishing a seed
does not overwrite existing pods; create a new pod to receive that generation.

By default, commits leave a pod only after explicit approval. A normal
`git push` from a configured checkout uses the pod's local Git binary and
configuration, but its receive-pack transport is routed through guestd to
hostd. Hostd stages the proposed branch and tag updates in a fresh namespace
inside the workspace's existing object cache, then evaluates the configured
Git policy. No repository copy is created and no host credential enters the
VM.

An allowed push is published immediately and a denied push fails without
creating an approval. A push requiring approval prints its pending identifier
and waits while the proposed updates are inspected, approved, or rejected from
the Repositories tab in the workspace sidebar. Approval starts publication as
a background host task, so the UI can resolve other requests concurrently.
`git push` exits successfully only after upstream publication completes and
exits unsuccessfully after rejection or a terminal failure. Interrupting the
waiting command does not discard its durable approval.

Multi-ref pushes remain atomic: denial of any update rejects the complete push,
any approval requirement retains the complete push, and automatic publication
occurs only when every update is allowed.

Approval publishes exactly the retained objects with leases against the
upstream state observed while staging; a concurrent upstream change therefore
fails closed. Rejection removes the namespaced refs without contacting the
upstream. Dirty or uncommitted files are never captured.

## Shared Caches

Each `[[caches]]` entry names a persistent workspace-level Btrfs subvolume and
mounts it read/write into every pod. `path` may be an absolute pod path or use
`~`/`~/...`, which is resolved against the pinned image user's `HOME`. The
stable `name` controls the backing subvolume, so removing a declaration does
not delete its contents and re-adding it makes the cache visible again. Caches
are intentionally a trust boundary between pods: any pod using the workspace
can modify or poison shared content. Runtime-owned paths such as `/nix`,
`/run`, `/proc`, and `/var/lib/docker`, plus overlapping cache destinations,
are rejected.

## Operations and State

Start the installed per-user daemon, then use the UI to create pods, run
terminals and agents, inspect changes, and approve repository publications:

```console
tascarrel daemon start
```

The CLI intentionally remains a small maintenance surface for the app, install,
doctor, daemon, and workspace lifecycle. Workspace commands use the same typed
host control plane as the UI:

```console
tascarrel workspace create demo
tascarrel workspace list
tascarrel workspace start demo
tascarrel workspace info demo --json
tascarrel workspace stop demo
tascarrel workspace delete demo
```

The selected workspace policy applies to every pod created in its dedicated
VM. Root inside a pod maps to an unprivileged, never-reused ID range in the
guest VM.

Tascarrel retains a bounded 64 KiB tail of each pod's startup stdout and stderr.
If startup fails before the pod can be retained, the relevant tail is included
in the creation error. The UI exposes pod health, startup diagnostics, process
logs, and workspace VM logs.

The host daemon exposes one private local control-plane socket at
`$TASCARREL_HOME/state/runtime/control.sock`. Each VM has a separate, private
QEMU chardev socket below that runtime directory; clients never connect to
those VM sockets directly. Persistent Btrfs state disks live below
`$TASCARREL_HOME/state/workspaces/<name>`; each VM uses an immutable system image
and an ephemeral tmpfs root. All host daemon state remains owned by the regular
user.

This prototype does not migrate incompatible state formats. Stop the daemon and
remove incompatible workspace state when required; no automatic migration or
rollback layer is provided yet.

## Development and Verification

```console
nix develop
cargo test --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
nix flake check
```

Build either native VM image explicitly with:

```console
nix build .#packages.x86_64-linux.vm-image
nix build .#packages.aarch64-linux.vm-image
```

`nix build .#tascarrel` builds the installable single-binary distribution with its
embedded guest payload. `nix build .#tascarrel-cli` builds only the host CLI and
daemon for development. `tascarrel-distribution` remains an alias for `tascarrel`.

For guest development, Cargo-built `tascarrel-guest` and `tascarrel-podd` binaries
can replace their image versions for one VM boot. The host exposes the supplied
directory read-only through virtiofs on Linux, with a 9p fallback and the same
9p transport on macOS. The binaries themselves must target Linux and the guest
architecture:

```console
nix develop --command cargo build -p tascarrel-guest -p tascarrel-podd
nix run .#host -- --local-binaries "$PWD/target/debug"
```

The packaged host command extracts and supplies the image, kernel, and initrd;
only the two shared executables override the immutable image for that boot. On
macOS, build or cross-compile Linux aarch64 binaries before using this option.

Tascarrel is licensed under either Apache-2.0 or MIT, at your option.
