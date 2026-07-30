{
  lib,
  pkgs,
  guestModule,
  portName,
  sharefsSmoke,
}:

let
  dataDevice = "/dev/disk/by-id/virtio-tascarrel-data";
  foreignHello = pkgs.pkgsCross.aarch64-multiplatform.pkgsStatic.hello;
  guestInstanceId = "guest_instance_1111111111111111111111";
in
pkgs.testers.runNixOSTest {
  name = "tascarrel-guest-module";
  qemu.forceAccel = true;

  nodes.machine =
    {
      lib,
      pkgs,
      ...
    }:
    let
      fakeGuest = pkgs.writeShellApplication {
        name = "tascarrel-guest";
        text = ''
          test -c "$TASCARREL_GUEST_DEVICE"
          test -x "$TASCARREL_GUEST_IP"
          test -x "$TASCARREL_GUEST_LOGIN_SHELL"
          test -x "$TASCARREL_GUEST_TERMINAL_SHELL"
          # The nested zsh, not this shell, must expand SHELL.
          # shellcheck disable=SC2016
          test "$("$TASCARREL_GUEST_TERMINAL_SHELL" -c 'test -r "$STARSHIP_CONFIG"; test -r "$ZDOTDIR/.zshrc"; printf %s "$SHELL"')" = ${lib.getExe pkgs.zsh}
          test -x "$TASCARREL_GUEST_NFT"
          test -x "$TASCARREL_GUEST_PODMAN"
          test -x "$TASCARREL_GUEST_NEWUIDMAP"
          test -x "$TASCARREL_GUEST_NEWGIDMAP"
          test -x "$TASCARREL_GUEST_TASCI"
          test "$TASCARREL_GUEST_INSTANCE_ID" = ${guestInstanceId}
          test "$TASCARREL_GUEST_STATE_DIR" = /var/lib/tascarrel
          test "$TASCARREL_GUEST_POD_NIX_DAEMON_SOCKET_DIR" = /run/tascarrel/pod-nix-daemon
          test -x "$TASCARREL_GUEST_BINDFS"
          test "$(stat -c '%a:%u:%g' /var/lib/tascarrel/nix-store/nix/var/nix/gcroots/tascarrel/pods)" = 700:0:0
          test "$(stat -c '%a:%u:%g' /var/lib/tascarrel/nix-store/nix/var/nix/tascarrel-gc-root-trash)" = 700:0:0
          test -x "$(command -v btrfs)"
          test -x "$(command -v buildkitd)"
          test -x "$(command -v bindfs)"
          test -x "$(command -v fuse-overlayfs)"
          test -x "$(command -v runc)"
          test -x "$(command -v umoci)"
          printf '%s\n' "$TASCARREL_GUEST_DEVICE" > "$TASCARREL_GUEST_RUNTIME_DIR/ready"
          exec sleep infinity
        '';
      };
      fakeTasci = pkgs.writeShellApplication {
        name = "tasci-exec";
        text = "exit 0";
      };
    in
    {
      imports = [ guestModule ];

      services.tascarrel-guest = {
        enable = true;
        package = fakeGuest;
        podctlPackage = fakeGuest;
        poddPackage = fakeGuest;
        tasciPackage = fakeTasci;
        networkIsolation = true;
        dataDevice = dataDevice;
        autoFormatDataDevice = true;
      };

      virtualisation.emptyDiskImages = [
        {
          size = 4096;
          driveConfig.deviceExtraOpts.serial = "tascarrel-data";
        }
      ];
      boot.kernelParams = [ "tascarrel.guest-instance-id=${guestInstanceId}" ];
      virtualisation.fileSystems."/var/lib/tascarrel" = {
        device = dataDevice;
        fsType = "btrfs";
        autoFormat = true;
        options = [ "discard=async" ];
      };

      # The isolation option must replace, rather than merge with, nameservers
      # supplied elsewhere in a reusable-module consumer.
      networking.nameservers = [ "9.9.9.9" ];

      virtualisation.qemu.options = [
        "-device virtio-serial-pci,id=tascarrel-serial"
        "-chardev null,id=tascarrel-control"
        "-device virtserialport,bus=tascarrel-serial.0,nr=1,chardev=tascarrel-control,name=${portName}"
      ];
      virtualisation.vlans = [ ];
      virtualisation.qemu.networkingOptions = lib.mkForce [ "-nic none" ];

      documentation.enable = false;
      environment.defaultPackages = lib.mkForce [ ];
      programs.command-not-found.enable = false;

      system.stateVersion = "24.11";
    };

  testScript = ''
    machine.start()
    machine.wait_for_unit("basic.target")
    machine.succeed("""
      set -euo pipefail
      lower=/run/tascarrel/sharefs-smoke-lower
      state=/var/lib/tascarrel/sharefs-smoke-state
      mountpoint=/run/tascarrel/sharefs-smoke-mount
      mkdir -p "$lower" "$mountpoint"
      btrfs subvolume create "$state"
      timeout --signal=KILL 20s ${lib.getExe' sharefsSmoke "sharefs-smoke"} "$lower" "$state" "$mountpoint" ${lib.getExe' pkgs.btrfs-progs "btrfs"}
      ! mountpoint -q "$mountpoint"
      test "$(cat "$lower/document")" = base
      test "$(cat "$lower/host-later")" = host
    """)
    machine.succeed("""
      set -euo pipefail
      share_root=/run/tascarrel/bindfs-idmap-test
      namespace_pid=
      cleanup_share_test() {
        mountpoint -q "$share_root/idmapped" && umount "$share_root/idmapped" || true
        test -z "$namespace_pid" || kill "$namespace_pid" || true
        mountpoint -q "$share_root/view" && umount "$share_root/view" || true
      }
      trap cleanup_share_test EXIT
      install -d "$share_root/source" "$share_root/view" "$share_root/idmapped"
      touch "$share_root/source/probe"
      bindfs_program=$(systemctl show tascarrel-guest.service -P Environment | grep -o 'TASCARREL_GUEST_BINDFS=[^ ]*' | cut -d= -f2-)
      test -x "$bindfs_program"
      "$bindfs_program" --force-user=0 --force-group=0 --perms=a+rwX "$share_root/source" "$share_root/view"
      initial_user_namespace=$(readlink /proc/self/ns/user)
      unshare --user --map-root-user -- sleep infinity >/dev/null 2>&1 &
      namespace_pid=$!
      for _ in $(seq 1 50); do
        child_user_namespace=$(readlink "/proc/$namespace_pid/ns/user" 2>/dev/null || true)
        test -z "$child_user_namespace" || test "$child_user_namespace" = "$initial_user_namespace" || break
        sleep 0.1
      done
      test -n "$child_user_namespace"
      test "$child_user_namespace" != "$initial_user_namespace"
      mount --bind --map-users "/proc/$namespace_pid/ns/user" -- "$share_root/view" "$share_root/idmapped"
      test -r "$share_root/idmapped/probe"
    """)
    machine.wait_for_unit("tascarrel-guest.service")
    machine.wait_for_unit("apparmor.service")
    machine.wait_for_unit("nftables.service")
    machine.succeed("grep -Fx 'tascarrel-pod (enforce)' /sys/kernel/security/apparmor/profiles")
    machine.succeed("grep -Fx 'tascarrel-pod-containers (enforce)' /sys/kernel/security/apparmor/profiles")
    ${pkgs.lib.optionalString pkgs.stdenv.hostPlatform.isx86_64 ''
      machine.wait_for_unit("systemd-binfmt.service")
      machine.succeed("grep -q '^flags:.*F' /proc/sys/fs/binfmt_misc/aarch64-linux")
      machine.succeed("${pkgs.lib.getExe pkgs.file} -Lb /run/binfmt/aarch64-linux | grep -F 'statically linked'")
      machine.succeed("${pkgs.lib.getExe foreignHello} | grep -Fx 'Hello, world!'")
      machine.succeed("mkdir /tmp/binfmt-root; cp ${pkgs.lib.getExe foreignHello} /tmp/binfmt-root/hello; chroot /tmp/binfmt-root /hello | grep -Fx 'Hello, world!'")
    ''}
    machine.succeed("test -c /dev/virtio-ports/${portName}")
    machine.succeed("test \"$(cat /run/tascarrel/ready)\" = /dev/virtio-ports/${portName}")
    machine.succeed("grep -Fx 'nameserver 192.0.2.53' /etc/resolv.conf")
    machine.succeed("test \"$(grep -c '^nameserver ' /etc/resolv.conf)\" = 1")
    machine.fail("grep -Eq '^options .*use-vc' /etc/resolv.conf")
    machine.succeed("test -c /dev/net/tun")
    machine.succeed("test \"$(cat /proc/sys/net/ipv4/ip_unprivileged_port_start)\" = 1024")
    machine.succeed("test \"$(ls -1 /sys/class/net)\" = lo")
    machine.fail("systemctl is-enabled dhcpcd.service")
    machine.fail("systemctl is-enabled nscd.service")
    machine.succeed("systemctl is-enabled tascarrel-pod-nix-gc.timer")
    machine.succeed("${pkgs.nix}/bin/nix config show experimental-features | grep -qw nix-command")
    machine.succeed("${pkgs.nix}/bin/nix config show experimental-features | grep -qw flakes")
    machine.succeed("systemctl show tascarrel-pod-nix-daemon.service -P ExecStart | grep -F -- '--option min-free 536870912'")
    machine.succeed("systemctl show tascarrel-pod-nix-daemon.service -P ExecStart | grep -F -- '--option max-free 1073741824'")
    machine.succeed("systemctl cat tascarrel-pod-nix-gc.timer | grep -Fx 'Persistent=true'")
    machine.succeed("systemctl cat tascarrel-pod-nix-gc.timer | grep -Fx 'RandomizedDelaySec=6h'")
    machine.succeed("test \"$(systemctl show tascarrel-pod-nix-gc.service -P Nice)\" = 10")
    machine.succeed("btrfs subvolume show /var/lib/tascarrel/nix-store >/dev/null")
    machine.succeed("test -S /run/tascarrel/pod-nix-daemon/socket")
    machine.succeed("printf tascarrel-periodic-gc-probe > /tmp/tascarrel-periodic-gc-probe; ${pkgs.nix}/bin/nix-store --store unix:///run/tascarrel/pod-nix-daemon/socket --add /tmp/tascarrel-periodic-gc-probe > /tmp/tascarrel-periodic-gc-path")
    machine.succeed("test -e /var/lib/tascarrel/nix-store$(cat /tmp/tascarrel-periodic-gc-path)")
    machine.succeed("systemctl start tascarrel-pod-nix-gc.service")
    machine.succeed("test ! -e /var/lib/tascarrel/nix-store$(cat /tmp/tascarrel-periodic-gc-path)")
    machine.fail("${pkgs.lib.getExe pkgs.nftables} list table inet tascarrel")
    machine.succeed("${pkgs.lib.getExe pkgs.nftables} add table inet tascarrel_destroy_probe")
    machine.succeed("printf 'destroy table inet tascarrel_destroy_probe\\n' | ${pkgs.lib.getExe pkgs.nftables} -f -")
    machine.fail("${pkgs.lib.getExe pkgs.nftables} list table inet tascarrel_destroy_probe")
  '';
}
