{
  pkgs,
  guestModule,
  portName,
}:

let
  dataDevice = "/dev/disk/by-id/virtio-tascarrel-data";
  foreignSystem = if pkgs.stdenv.hostPlatform.isx86_64 then "aarch64-linux" else "x86_64-linux";
  foreignHello =
    if pkgs.stdenv.hostPlatform.isx86_64 then
      pkgs.pkgsCross.aarch64-multiplatform.pkgsStatic.hello
    else
      pkgs.pkgsCross.gnu64.pkgsStatic.hello;
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
          test "$TASCARREL_GUEST_INSTANCE_ID" = ${guestInstanceId}
          test "$TASCARREL_GUEST_STATE_DIR" = /var/lib/tascarrel
          test "$TASCARREL_GUEST_POD_NIX_DAEMON_SOCKET_DIR" = /run/tascarrel/pod-nix-daemon
          test "$(stat -c '%a:%u:%g' /var/lib/tascarrel/nix-store/nix/var/nix/gcroots/tascarrel/pods)" = 700:0:0
          test "$(stat -c '%a:%u:%g' /var/lib/tascarrel/nix-store/nix/var/nix/tascarrel-gc-root-trash)" = 700:0:0
          test -x "$(command -v btrfs)"
          test -x "$(command -v buildkitd)"
          test -x "$(command -v fuse-overlayfs)"
          test -x "$(command -v runc)"
          test -x "$(command -v umoci)"
          printf '%s\n' "$TASCARREL_GUEST_DEVICE" > "$TASCARREL_GUEST_RUNTIME_DIR/ready"
          exec sleep infinity
        '';
      };
    in
    {
      imports = [ guestModule ];

      services.tascarrel-guest = {
        enable = true;
        package = fakeGuest;
        podctlPackage = fakeGuest;
        poddPackage = fakeGuest;
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
    machine.wait_for_unit("tascarrel-guest.service")
    machine.wait_for_unit("apparmor.service")
    machine.wait_for_unit("nftables.service")
    machine.wait_for_unit("systemd-binfmt.service")
    machine.succeed("grep -Fx 'tascarrel-pod (enforce)' /sys/kernel/security/apparmor/profiles")
    machine.succeed("grep -Fx 'tascarrel-pod-containers (enforce)' /sys/kernel/security/apparmor/profiles")
    machine.succeed("grep -q '^flags:.*F' /proc/sys/fs/binfmt_misc/${foreignSystem}")
    machine.succeed("${pkgs.lib.getExe pkgs.file} -Lb /run/binfmt/${foreignSystem} | grep -F 'statically linked'")
    machine.succeed("${pkgs.lib.getExe foreignHello} | grep -Fx 'Hello, world!'")
    machine.succeed("mkdir /tmp/binfmt-root; cp ${pkgs.lib.getExe foreignHello} /tmp/binfmt-root/hello; chroot /tmp/binfmt-root /hello | grep -Fx 'Hello, world!'")
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
