# Tascarrel VM Images

The flake builds minimal NixOS EROFS system images for
`x86_64-linux` and `aarch64-linux`:

```console
nix build .#packages.x86_64-linux.vm-image
nix build .#packages.aarch64-linux.vm-image
```

The `packages.<system>.tascarrel` output builds the matching Linux system image,
kernel, and initrd and packs them into one SHA-256-addressed, xz-compressed tar
payload embedded in the final Tascarrel binary. The
`packages.<system>.tascarrel-cli` output builds the administrative client and
server without that payload for development, while the
`tascarrel-distribution` output aliases the complete `tascarrel` package.
Additional assets can be added to the archive without changing the embedding
scheme.

The release pipeline builds each portable payload archive on a matching native
runner through `packages.x86_64-linux.guest-payload` and
`packages.aarch64-linux.guest-payload`. Each output contains `payload.tar.xz`,
`payload.sha256`, `payload.size`, and `architecture`. The host Cargo build wraps
the archive in an object for its own target, allowing the same AArch64 guest
payload to feed both AArch64 Linux and Apple Silicon macOS releases.

Linux distributions use a statically linked musl executable; macOS remains one
executable while linking the platform system libraries.

The `tascarrel-desktop` Nix package combines the Tascarrel server with a thin
Electron window and installs a freedesktop application entry. Closing the
window does not stop the server. Build `tascarrel-desktop-appimage` when the
result must run on Debian, Ubuntu, or another Linux distribution without Nix:

```console
nix build .#tascarrel-desktop-appimage
```

This AppImage embeds the desktop application's complete Nix store closure and
requires FUSE 3 and enabled user namespaces on the target system.

The `tascarrel` server verifies and extracts its embedded payload below
`$TASCARREL_HOME/state/payloads/<sha256>/`, removes older generations, and starts
the regular-user host supervisor. The supervisor creates a dedicated VM lazily
when a typed operation first needs a workspace guest. The `aarch64-darwin` build carries
the aarch64 Linux guest and runs it under QEMU/HVF on Apple Silicon. QEMU loads
the matching extracted kernel and initrd directly, bypassing guest firmware and
the disk bootloader. Only QEMU and hardware virtualization are required from
the host.

## VM Devices and Persistent State

Each managed workspace VM has only these Tascarrel-owned external connections:

- a read-only EROFS Nix store image with an ephemeral tmpfs root;
- a sparse persistent state disk, mounted as Btrfs at `/var/lib/tascarrel`, which
  holds image/pod state, disk-backed image-build scratch, and the separate
  workspace-wide pod Nix store;
- its private `tascarrel-control` virtio-serial chardev and port.

QEMU is launched with `-nic none` and no virtual network backend. Additional
QEMU arguments which could introduce a NIC, network backend, disk, filesystem,
or external configuration are rejected. Workspace files are not mounted or
otherwise exposed to QEMU.

The host supervisor publishes exactly one private typed control-plane socket at
`$TASCARREL_HOME/state/runtime/control.sock`. The `tascarrelctl` administrative
client uses it for host-owned workspace lifecycle actions; guest-owned UI
operations start the selected VM lazily through hostd. Persistent state lives below
`$TASCARREL_HOME/state/workspaces/<name>`, and each QEMU virtio-serial socket
remains private below the Tascarrel runtime directory. Local clients never
connect directly to a VM chardev.

`TASCARREL_HOME` defaults to `$HOME/.tascarrel` on Linux and macOS. Native
aarch64 guests use HVF automatically; TCG remains available as an explicit
fallback.

The Nix store and generated `/etc` are immutable. The root, `/run`, `/tmp`,
logs, and caches are ephemeral; only `/var/lib/tascarrel` uses the persistent
state disk. The EROFS image also carries registration metadata for its store
closure. On every boot, the guest loads that metadata into the VM Nix daemon's
transient database before starting the daemon. The guest then uses the daemon as
the source for `nix copy`, ensuring that the persistent workspace-wide pod store
has the current runtime closure; existing paths are reused. After stopping the
host daemon, restart it with
`--reset-state-workspace <name>` to discard that workspace's persistent state
disk on its first lazy start. `TASCARREL_STATE_DISK_SIZE` sets the default size
requested for workspace state disks; `[vm].disk` overrides it per workspace.
Increasing either configured size grows both an existing sparse image and its
Btrfs filesystem on the next VM start. Decreasing it never shrinks either one.

## Workspace Image Context

For a selected workspace, the managed supervisor reads:

```text
$TASCARREL_HOME/config/workspaces/<name>/config.toml
$TASCARREL_HOME/config/workspaces/<name>/image/
$TASCARREL_HOME/config/workspaces/<name>/<configured overlay>/
```

On every image fingerprint or build, hostd packs the current
config, image context, and configured overlay into one bounded snapshot and
streams it over a flow-controlled virtio-serial mux channel. The guest validates
and publishes the complete generation atomically before using it. Host edits
therefore reach the next image build without a VM restart. The
guest validates the OCI result and publishes immutable image and workspace
generations on its Btrfs state disk.

For development, `tascarrel --local-binaries <directory>` exposes a
read-only directory containing `tascarrel-guest` and `tascarrel-podd` to the VM.
The guest mounts it through virtiofs or portable 9p and bind-mounts both
executables over their image paths for that boot. The files must be executable
Linux binaries for the guest architecture.

## Guest Networking

The appliance runs no DHCP client and receives no QEMU network interface. The
guest daemon owns the synthetic `tascarrel0` route, per-pod veths, transparent
DNS/TCP listeners, and the atomic nftables ruleset. Guestd terminates DNS and
sends bounded semantic questions over the workspace mux for hostd's network
service to resolve. Attributed TCP uses a separate mux stream and switches to
raw relay after host admission. Guest-local and inter-pod traffic is rejected
by default, as is arbitrary external UDP.

Pods use the reserved DNS address `192.0.2.53`; TCP and UDP port 53 are both
captured regardless of the resolver address selected by the workload. Hostd
uses the host resolver from `/etc/resolv.conf`, or `tascarrel
--dns-resolver`. External IPv6 TCP/UDP egress is not yet implemented.

## Modules and Checks

The reusable `nixosModules.tascarrel-guest` module requires both the guest daemon
package and the `tascarrel-podd` package. Consumers must explicitly enable its
network-isolation policy and may configure a dedicated data device. The
packaged appliance supplies these settings automatically.

The `nix flake check` command builds the packaged components, checks formatting,
and boots a focused NixOS VM test for this module.

```console
nix flake check
```
