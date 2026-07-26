<p align="center">
  <img src="frontend/src/assets/tascarrel.svg" width="72" alt="Tascarrel logo">
</p>

<h1 align="center">Tascarrel</h1>

<p align="center">
  <strong>Agentic Development Workbench Where Agents Work Safely Without Babysitting</strong>
</p>

<p align="center">
  <a href="https://tascarrel.dev/docs/getting-started/installation">Install Tascarrel</a> ·
  <a href="https://tascarrel.dev/docs">Read the Docs</a>
</p>

<p align="center">
  <img
    src="https://tascarrel.dev/img/tascarrel-workbench.png"
    alt="Tascarrel showing workspace and task navigation, a coding agent session, and a live application preview side by side"
  >
</p>

Tascarrel is an agentic development workbench that lets agents work safely
without babysitting. You declare each development context—its repositories,
tools, resources, and access policies—and Tascarrel runs it in a workspace
virtual machine isolated from your host and other workspaces. Within that safety
boundary, agents can use broad permissions in disposable task pods instead of
stopping for command-by-command approvals. Each pod has its own writable files,
processes, and network namespace, so agents working in parallel do not trip over
one another. The UI keeps you on top of their sessions, shows what needs your
attention, and lets you review changes before publishing them through Git.

> [!CAUTION]
> **Tascarrel is experimental.** It may change or break without notice. Do not
> entrust it with important or confidential data. Use it only with data you can
> afford to lose or leak.

## Highlights

- **Bring Your Subscriptions** — Sign in with ChatGPT or connect Claude Code.
- **Layered VM and Task Isolation** — VMs isolate workspaces; pods separate tasks.
- **Controlled Network Access** — Set egress rules and expose services deliberately.
- **Host-Injected Credentials** — Keep Git and injected HTTP credentials on the host.
- **Agent-Agnostic** — Use Codex, Claude Code, or Tascarrel's own agent in one UI.
- **Many Agents at Once** — Switch and steer parallel tasks in one UI.
- **Autonomy through Isolation** — Run agents without permission prompts.
- **Local First** — Keep environments and sessions on your hardware.
- **Shareable Workspaces** — Track reusable workspace configuration in Git.
- **Multi-Repository Workspaces** — Work across every repository at once.
- **Fast, Fresh Tasks** — Reuse prepared images, snapshots, and caches.

## Isolation Model

The workspace VM is the safety boundary. Pods provide lightweight task
isolation, but share the VM's kernel and are not security boundaries from one
another. Use separate workspaces for projects with different trust requirements.

[Read the isolation model](https://tascarrel.dev/docs/getting-started/isolation-model)
for its guarantees, limitations, and residual risks.

Tascarrel is available under the
[Apache License 2.0](LICENSE-APACHE) or [MIT License](LICENSE-MIT).
