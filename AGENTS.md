This is Tascarrel, a local-first agentic development environment.

Tascarrel is designed such that users can do cross-project development
within one interface while staying on-top of it. Projects are organized
into *workspaces*. A workspace consists of a collection of repositories
(the projects) and an environment definition. Each workspace has a name
that serves as its identity. For each workspace, Tascarrel spins up a VM
to isolate the workspace from the host and other workspaces. A *pod* is
an isolated, disposable environment for one task within a workspace. Pods
within a workspace are isolated using Linux namespaces. Each VM runs a
dedicated supervisor (`guestd`) for pods within a VM.

Tascarrel targets macOS and Linux (Linux is the focus for now).

The users explicit instructions take priority over this file. If you are
not sure, ask the user explicitly in case of conflicts.

We use `nix` for development. Run commands through `nix develop`.

This document describes the desired style, architecture, and properties.
The implementation may not always follow it. Don't rewrite parts not
relevant to your task just because they don't conform.

When explicitly told to work on a specific part, assume that concurrent
work to other parts may take place. Take feasible measures to isolate what
you are doing to the part you are told to work on.

DO NOT MODIFY THIS FILE UNLESS TOLD TO

# Architecture

- The host runs the Tascarrel host daemon (`hostd`) who is responsible for
  supervising and managing workspace VMs and their lifecycle.
- Each workspace VM runs the guest daemon (`guestd`) who is responsible
  for supervising and managing pods and their lifecycle.
- Each pod runs the pod daemon (`podd`) who is responsible for supervising
  and managing processes within the pod.
- The host daemon provides the HTTP API.
- Every interaction with Tascarrel as a whole flows through the host daemon.
- Each daemon owns a well-defined slice of the state of the system.
- The host daemon serves as a cache-less gateway for its guest daemons.
- A guest daemon serves as a cache-less gateway for its pod daemons.

# Security

- Compromised VMs must never compromise the host or other VMs.
- Compromised pods must never compromise the VMs guest daemon.
- Compromised pods may compromise other pods in the same workspace VM,
  e.g., by poisoning of shared caches. Pods are not a strong security
  boundary, they just provide lightweight environment isolation.
- Hypervisor bugs may lead to a host breach (acceptable risk).
- Kernel bugs may lead to a guest daemon break (acceptable risk).
- Availability is explicitly out-of-scope. A compromised pod may DoS
  the guest and host by consuming resources (e.g., compute, disk).

# APIs

Tascarrel uses two API patterns:

- RPC: A client may invoke an *action* that produces some *output*.
- Subscriptions: A client may subscribe to an event stream.

Action = Procedure Name + Input

Always use subscriptions for querying live state owned by another component.
Live state should always be exposed through subscriptions. In addition, RPCs
for point-in-time snapshots may be used (in particular by `podctl`).

Use RPC APIs for mutations and for querying immutable state.

There are four kinds of errors:

- *Internal errors* are failures beyond the control of the consumer.
- *Contract errors* are violations of preconditions/invariants by the consumer.
- *Transport errors* are failures of the underlying transport primitives.
- *Domain errors* are application-level failures.

Only domain errors belong into the RPC and subscription/event types.

In addition, there may be HTTP and internal streaming APIs (e.g., for
transmitting large file blobs and other payloads).

If available render `reportify` error reports into internal and contract errors.

API types never use `kebab-case`. `config.sidex` is not API but TOML configuration.
It always uses `kebab-case` for field names but not for variants.

- Group and order definitions as follows: Each action directly followed by its
  output type, each subscription directly followed by its event type, domain types.
  Add the following delimiters: // RPC, // Subscriptions // Domain Types
- Put IDs at the top of the domain types.
- Subscription type docs should state what the client subscribes to (“Subscribes to...”).
- Always use `record` for outputs and events.
- Add documentation for ALL fields and and parts of a schema.

# Testing

- Only run expensive tests when a change may have broken them.

# Coding Guidelines

- Never write unit tests for trivially correct implementations.
- Imports always belong at the top of a file.
- Never write inline comments that explain what something does.
- Only write inline comments in cases where (a) it's not obvious why
  something is correct or (b) a decision record is required.
- Prefer specific names over generic ones.
- Modularize code where appropriate.
- Use Sidex for API (JSON) and configuration (JSON and TOML) types.
- Never hand-roll foundational libraries (e.g., for HTTP or JSON).
- Introduce dependencies only after carefully vetting them.
- If functionality isn't foundational and can be written in ~1000
  lines, prefer to build it yourself as a reusable library.
- Never edit generated code by hand. Instead rerun code generation.
- Avoid hard-coded constants unless appropriate. In particular, don't
  hard-code limits and paths. Instead, hard-code defaults.
- Document internal helpers where appropriate.
- Documentation should explain important concepts in the abstract.
- Don't write documentation that explains how to use something if you
  can instead just give a concrete code example.
- Sentences in docs and prose must not start with lowercase words.

Order definitions by their importance, not by dependency order:

1) Primary public types and their implementations.
2) Supporting public types (e.g., builders, options, errors, consts).
3) Private implementation types and helper functions.

## Rust

- Never use `pub(super)`. Use either `pub` or `pub(crate)`.
- Use `reportify` whenever something returns an error. Avoid putting
  secrets in reports. If you have to, use `reportify`'s secret redaction
  functionality to make sure they are handled appropriately.
- Use `thiserror` in combination with `reportify` to define error types
  that need handling by the caller. When using `thiserror` do not blindly
  expose all errors but only those (categories) that a caller my plausibly
  want to handle.
- Error messages should state the failure (e.g., “error XYZ” or “failed to XYZ”).
- Crates and modules must have top-level documentation that explains their
  purpose, scope, and the primary public interface.
- When adding something to a crate/module, consider whether it fits the
  documented scope. If not, extend the scope only if reasonable. Otherwise,
  create a new crate/module for what you wanted to add.
- Never use `json!` or direct parsing of `serde_json::Value`. Always
  define proper types implementing Serde's traits.
- If a module has submodules, use a directory with `mod.rs`.
- Binaries belong in `crates/apps`, libraries in `crates/libs`.
- Use `tracing` for logging.
- Instrument key functional boundaries with `tracing::instrument` with an
  appropriate level an arguments.
- Never ignore errors without at least logging them.
- Aim to keep the dependency hierarchy between crates flat.
- Dependencies should be declared on the workspace level. Crates may add
  features they rely on on an individual level.
- Avoid using lifetimes other than `'static` unless necessary.
- Document internal helpers (but keep it brief).
- Put unit tests at the end of the respective module, not in a separate file.
- Imports only required for tests must be imported within the test module, not
  at the top of the file. Avoid using `#[cfg(test)]` outside of tests.
- Avoid using `no_run`. Prefer runnable examples.
- Do not add module-level docs to `tests` modules.
- To each test, add a brief description of what the test tests.
- Only use `unwrap` in tests. Outside of tests, use Rust's `except` and state the
  invariant that provides the guarantee in the message.
- Internal services in the daemons must not take other services as dependencies
  at construction time, instead, these services should be provided when invoking
  operations within a service that require them (see `guestd`).

## TypeScript and React

- Follow a feature-based architecture.
- Don't mix feature and type directories.
- Always write and use reusable components for UI elements.
- Before writing a component, look for existing components.
- Make sure to follow accessibility standards.
- Follow and use the design system of the project.
- Never add purely decorative elements (e.g., icons, small circles).
- Icons must be used consistently and to improve scanning for the user. Use
  icons for important concepts or actions, not for decoration.

## Documentation

- Capitalize headings according to the Chicago Manual of Style.

## Autonomy

- If something needs user attention, try to make progress on other items first.
- Collect blockers that do need user attention. Provide the user with a list of
  these blockers when you are done. For each blocker, provide a concise description
  (not more than a few sentences), possible options, as well as a recommendation
  and your reasoning for the recommendation.