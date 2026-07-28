{
  config,
  lib,
  pkgs,
  ...
}:

let
  inherit (lib)
    mkEnableOption
    mkIf
    mkOption
    types
    ;
  cfg = config.services.tascarrel-guest;
  system = pkgs.stdenv.hostPlatform.system;
  emulatedSystems =
    if system == "x86_64-linux" then
      [ "aarch64-linux" ]
    else if system == "aarch64-linux" then
      [ ]
    else
      throw "Tascarrel guests do not support ${system}";
  dataDiskDevice = "/dev/disk/by-id/virtio-tascarrel-data";
  stateDirectory = "/var/lib/tascarrel";
  podNixRoot = "${stateDirectory}/nix-store";
  podNixStore = "${podNixRoot}/nix/store";
  podNixState = "${podNixRoot}/nix/var/nix";
  podNixSocketDirectory = "/run/tascarrel/pod-nix-daemon";
  podNixSocket = "${podNixSocketDirectory}/socket";
  localBinariesDirectory = "/run/tascarrel/local-binaries";
  localBinariesMountTag = "tascarrel-binaries";
  guestBinary = lib.getExe' cfg.package cfg.binary;
  podctlBinary = lib.getExe' cfg.podctlPackage "podctl";
  poddBinary = lib.getExe' cfg.poddPackage "tascarrel-podd";
  tasciBinary = lib.getExe' cfg.tasciPackage "tasci-exec";
  tascarrelStarshipConfig = pkgs.writeText "tascarrel-starship.toml" ''
    add_newline = false
    command_timeout = 1000

    [character]
    success_symbol = "[❯](bold green)"
    error_symbol = "[❯](bold red)"

    [directory]
    truncation_length = 4
    truncate_to_repo = false
  '';
  tascarrelZshConfig = pkgs.writeTextDir ".zshrc" ''
    HISTFILE="''${HISTFILE:-$HOME/.zsh_history}"
    HISTSIZE=10000
    SAVEHIST=10000
    setopt HIST_IGNORE_DUPS SHARE_HISTORY INTERACTIVE_COMMENTS
    if (( $+commands[mise] )); then
      fpath=("$HOME/.local/share/zsh/site-functions" $fpath)
      autoload -Uz compinit
      compinit
    fi
    eval "$(${lib.getExe pkgs.starship} init zsh)"
  '';
  tascarrelTerminalShell = pkgs.writeShellScript "tascarrel-terminal-shell" ''
    export SHELL=${lib.escapeShellArg (lib.getExe pkgs.zsh)}
    export STARSHIP_CONFIG="''${STARSHIP_CONFIG:-${tascarrelStarshipConfig}}"
    export ZDOTDIR="''${ZDOTDIR:-${tascarrelZshConfig}}"
    exec ${lib.escapeShellArg (lib.getExe pkgs.zsh)} "$@"
  '';
  podAppArmorRules = ''
    # Pods already have private filesystems and PID namespaces. This profile
    # mediates the remaining kernel interfaces without constraining ordinary
    # development tools or the explicitly enabled device features.
    file,
    network,
    capability,
    ptrace,
    signal,
    unix,

    deny /proc/kcore r,
    deny /proc/sysrq-trigger w,
    deny /sys/firmware/{,**} rw,
  '';
  persistentPoddBinary = "${podNixStore}/${lib.removePrefix "/nix/store/" poddBinary}";
  persistentPodctlBinary = "${podNixStore}/${lib.removePrefix "/nix/store/" podctlBinary}";
  podNixRuntimePaths =
    lib.optionals (cfg.package != null) [ cfg.package ]
    ++ lib.optionals (cfg.poddPackage != null) [ cfg.poddPackage ]
    ++ lib.optionals (cfg.podctlPackage != null) [ cfg.podctlPackage ]
    ++ lib.optionals (cfg.tasciPackage != null) [ cfg.tasciPackage ]
    ++ [
      config.nix.package
      pkgs.bashInteractive
      tascarrelTerminalShell
      cfg.codeServerPackage
      pkgs.docker
      pkgs.podman
      pkgs.shadow
    ]
    ++ cfg.podNixStoreSeedPaths;
  podNixRuntimeClosure = pkgs.linkFarm "tascarrel-pod-nix-runtime" (
    lib.imap0 (index: path: {
      name = "${toString index}-${builtins.unsafeDiscardStringContext (baseNameOf path)}";
      inherit path;
    }) podNixRuntimePaths
  );
in
{
  options.services.tascarrel-guest = {
    enable = mkEnableOption "the Tascarrel guest daemon";

    package = mkOption {
      type = types.nullOr types.package;
      default = null;
      description = "Package containing the Tascarrel guest daemon.";
    };

    poddPackage = mkOption {
      type = types.nullOr types.package;
      default = null;
      description = "Package containing the immutable Tascarrel pod PID 1.";
    };

    podctlPackage = mkOption {
      type = types.nullOr types.package;
      default = null;
      description = "Package containing the immutable Tascarrel pod control client.";
    };

    tasciPackage = mkOption {
      type = types.nullOr types.package;
      default = null;
      description = "Package containing the immutable Tasci coding harness.";
    };

    codeServerPackage = mkOption {
      type = types.package;
      default = pkgs.code-server;
      defaultText = lib.literalExpression "pkgs.code-server";
      description = ''
        Immutable code-server closure mounted read-only at
        /opt/tascarrel/tools/code-server in every pod.
      '';
    };

    binary = mkOption {
      type = types.str;
      default = "tascarrel-guest";
      description = "Guest daemon executable name within the package.";
    };

    portName = mkOption {
      type = types.str;
      default = "tascarrel-control";
      description = "Name advertised by QEMU for the Tascarrel virtio-serial port.";
    };

    device = mkOption {
      type = types.path;
      readOnly = true;
      default = "/dev/virtio-ports/${cfg.portName}";
      description = "Guest path for the Tascarrel virtio-serial port.";
    };

    extraArgs = mkOption {
      type = types.listOf types.str;
      default = [ ];
      description = "Additional command-line arguments passed to the guest daemon.";
    };

    networkIsolation = mkEnableOption ''
      appliance-wide DHCP, DNS, nftables, and NSS isolation required for
      veth-attributed pod egress
    '';

    dataDevice = mkOption {
      type = types.nullOr types.str;
      default = null;
      example = dataDiskDevice;
      description = ''
        Dedicated block device mounted as Btrfs persistent Tascarrel storage.
        Set this only to a device whose lifecycle is owned by Tascarrel.
      '';
    };

    autoFormatDataDevice = mkEnableOption ''
      one-time NixOS auto-formatting of the dedicated tascarrel-data virtio disk
    '';

    podNixStoreSeedPaths = mkOption {
      type = types.listOf types.package;
      default = [ ];
      internal = true;
      description = ''
        Additional closures copied into the persistent pod Nix store. This is
        intended for runtime integration tests and extensions which inject
        immutable executables into pod mount namespaces.
      '';
    };

  };

  config = mkIf cfg.enable {
    # Downloaded harnesses may use the conventional glibc interpreter path
    # instead of a Nix store path. nix-ld supplies that interpreter and the
    # runtime libraries needed to execute them in the minimal guest image.
    programs.nix-ld.enable = true;

    security.apparmor = {
      enable = true;
      policies = {
        tascarrel-pod.profile = ''
          abi <abi/4.0>,
          include <tunables/global>

          profile tascarrel-pod flags=(attach_disconnected,mediate_deleted) {
            ${podAppArmorRules}
          }
        '';
        tascarrel-pod-containers.profile = ''
          abi <abi/4.0>,
          include <tunables/global>

          profile tascarrel-pod-containers flags=(attach_disconnected,mediate_deleted) {
            ${podAppArmorRules}
            userns,
            mount,
            umount,
            pivot_root,
          }
        '';
      };
    };

    # Builds trigger collection under pressure, while the persistent weekly
    # timer reclaims unreachable paths from otherwise short-lived workspace
    # VMs. Both thresholds are explicit: Nix's min-free default disables
    # pressure collection and an unbounded max-free would collect all garbage.
    nix.settings = {
      experimental-features = lib.mkDefault [
        "nix-command"
        "flakes"
      ];
      fsync-store-paths = true;
    };
    services.fstrim = mkIf (cfg.dataDevice != null) {
      enable = true;
      interval = "weekly";
    };

    # QEMU deliberately exposes no network device. The guest daemon owns its
    # synthetic route, veths, semantic DNS/TCP listeners, and nftables table so
    # they stay synchronized with pod state.
    networking = mkIf cfg.networkIsolation {
      useDHCP = false;
      dhcpcd.enable = false;
      nameservers = lib.mkForce [ "192.0.2.53" ];
      resolvconf.enable = lib.mkForce false;
      nftables.enable = true;
      # Tascarrel owns the appliance's complete dynamic nftables policy. The
      # generic NixOS firewall cannot know about the per-principal redirects
      # and pod veth pairs.
      firewall = {
        enable = lib.mkForce false;
        checkReversePath = lib.mkForce false;
      };
    };
    environment.etc."resolv.conf" = mkIf cfg.networkIsolation {
      text = "nameserver 192.0.2.53\n";
      mode = "0644";
    };
    boot.kernel.sysctl = mkIf cfg.networkIsolation {
      "net.ipv4.ip_unprivileged_port_start" = 1024;
      "net.ipv4.ip_forward" = 1;
    };
    # Keep name service local to each pod process. Every pod receives an
    # injected resolver file pointing at the virtual DNS address; shared NSS
    # caching is unnecessary in this small appliance.
    services.nscd.enable = mkIf cfg.networkIsolation false;
    system = mkIf cfg.networkIsolation {
      nssModules = lib.mkForce [ ];
      nssDatabases = {
        passwd = lib.mkForce [ "files" ];
        group = lib.mkForce [ "files" ];
        shadow = lib.mkForce [ "files" ];
        hosts = lib.mkForce [
          "files"
          "dns"
        ];
      };
    };

    assertions = [
      {
        assertion = cfg.package != null;
        message = "services.tascarrel-guest.package must be set when the service is enabled";
      }
      {
        assertion = cfg.poddPackage != null;
        message = "services.tascarrel-guest.poddPackage must be set when the service is enabled";
      }
      {
        assertion = cfg.podctlPackage != null;
        message = "services.tascarrel-guest.podctlPackage must be set when the service is enabled";
      }
      {
        assertion = cfg.tasciPackage != null;
        message = "services.tascarrel-guest.tasciPackage must be set when the service is enabled";
      }
      {
        assertion = cfg.dataDevice != null;
        message = "services.tascarrel-guest.dataDevice must name the persistent state disk";
      }
      {
        assertion = cfg.networkIsolation;
        message = ''
          services.tascarrel-guest.networkIsolation must be enabled to acknowledge
          the appliance-wide DHCP, DNS, nftables, and NSS policy required by
          veth-attributed pod egress
        '';
      }
      {
        assertion = builtins.match "^[A-Za-z0-9._-]+$" cfg.portName != null;
        message = "services.tascarrel-guest.portName may contain only ASCII letters, digits, '.', '_', and '-'";
      }
      {
        assertion = !cfg.autoFormatDataDevice || cfg.dataDevice == dataDiskDevice;
        message = ''
          services.tascarrel-guest.autoFormatDataDevice is restricted to the
          dedicated ${dataDiskDevice} device
        '';
      }
    ];

    boot = {
      binfmt = {
        inherit emulatedSystems;
        preferStaticEmulators = true;
      };
      supportedFilesystems = [
        "btrfs"
        "9p"
        "virtiofs"
      ];
      kernelModules = [
        "virtio_console"
        "virtiofs"
        "9p"
        "9pnet_virtio"
        "btrfs"
        "overlay"
        "fuse"
        "tun"
        "bridge"
        "br_netfilter"
        "veth"
        "nf_conntrack"
        "nf_nat"
      ];
    };

    fileSystems = lib.mkMerge [
      (mkIf (cfg.dataDevice != null) {
        "${stateDirectory}" = {
          device = cfg.dataDevice;
          fsType = "btrfs";
          autoFormat = cfg.autoFormatDataDevice;
          options = [
            "compress=zstd:3"
            "discard=async"
            "noatime"
            "x-systemd.growfs"
          ];
        };
      })
    ];

    services.udev.extraRules = ''
      SUBSYSTEM=="virtio-ports", ATTR{name}=="${cfg.portName}", OWNER="root", GROUP="root", MODE="0600"
    '';

    systemd.services.tascarrel-pod-nix-gc = {
      description = "Collect the persistent Tascarrel pod Nix store";
      requires = [
        "tascarrel-pod-nix-daemon.socket"
        "tascarrel-pod-nix-store.service"
      ];
      after = [
        "tascarrel-pod-nix-daemon.socket"
        "tascarrel-pod-nix-store.service"
      ];
      serviceConfig = {
        Type = "oneshot";
        ExecStart = "${lib.getExe config.nix.package} store gc --store unix://${podNixSocket} --max 4G";
        Nice = 10;
        IOSchedulingClass = "idle";
        IOSchedulingPriority = 7;
      };
    };
    systemd.timers.tascarrel-pod-nix-gc = {
      description = "Periodically collect the persistent Tascarrel pod Nix store";
      wantedBy = [ "timers.target" ];
      timerConfig = {
        OnCalendar = "weekly";
        Persistent = true;
        RandomizedDelaySec = "6h";
        Unit = "tascarrel-pod-nix-gc.service";
      };
    };

    systemd.services.tascarrel-pod-nix-store = {
      description = "Initialize the persistent Tascarrel pod Nix store";
      requires = [
        "nix-daemon.socket"
        "var-lib-tascarrel.mount"
      ];
      after = [
        "nix-daemon.socket"
        "var-lib-tascarrel.mount"
      ];
      before = [
        "tascarrel-guest.service"
        "tascarrel-pod-nix-daemon.service"
      ];
      unitConfig.RequiresMountsFor = [ stateDirectory ];
      path = [
        pkgs.btrfs-progs
        pkgs.coreutils
      ];
      script = ''
        set -euo pipefail
        if [[ ! -e ${lib.escapeShellArg podNixRoot} ]]; then
          btrfs subvolume create ${lib.escapeShellArg podNixRoot}
        elif [[ ! -d ${lib.escapeShellArg podNixRoot} ]] \
          || ! btrfs subvolume show ${lib.escapeShellArg podNixRoot} >/dev/null; then
          echo "persistent pod Nix store root is not a Btrfs subvolume: ${podNixRoot}" >&2
          exit 1
        fi
        chmod 0755 ${lib.escapeShellArg podNixRoot}
        ${lib.getExe config.nix.package} copy \
          --from daemon \
          --to ${lib.escapeShellArg podNixRoot} \
          --no-check-sigs \
          ${lib.escapeShellArg podNixRuntimeClosure}
        install -d -m 0700 \
          ${lib.escapeShellArg "${podNixState}/gcroots/tascarrel"} \
          ${lib.escapeShellArg "${podNixState}/gcroots/tascarrel/pods"} \
          ${lib.escapeShellArg "${podNixState}/tascarrel-gc-root-trash"}
        ln -sfnT ${lib.escapeShellArg podNixRuntimeClosure} \
          ${lib.escapeShellArg "${podNixState}/gcroots/tascarrel/runtime"}
      '';
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
        StandardOutput = "journal+console";
        StandardError = "journal+console";
      };
    };

    systemd.sockets.tascarrel-pod-nix-daemon = {
      description = "Persistent Tascarrel pod Nix daemon socket";
      requires = [
        "run-tascarrel.mount"
      ];
      after = [
        "run-tascarrel.mount"
      ];
      socketConfig = {
        ListenStream = podNixSocket;
        SocketMode = "0666";
        DirectoryMode = "0755";
        RemoveOnStop = true;
      };
    };

    systemd.services.tascarrel-pod-nix-daemon = {
      description = "Persistent Tascarrel pod Nix daemon";
      requires = [ "tascarrel-pod-nix-store.service" ];
      after = [ "tascarrel-pod-nix-store.service" ];
      serviceConfig = {
        ExecStart = ''
          ${lib.getExe config.nix.package} daemon \
            --store ${lib.escapeShellArg podNixRoot} \
            --extra-experimental-features daemon-trust-override \
            --option min-free ${toString (512 * 1024 * 1024)} \
            --option max-free ${toString (1024 * 1024 * 1024)} \
            --force-untrusted
        '';
        User = "root";
        Group = "root";
        LimitNOFILE = 1048576;
        TasksMax = "infinity";
      };
    };

    # Namespace handles must be pinned from PID 1's original mount namespace:
    # the kernel rejects nsfs mount-namespace pins which could form a namespace
    # loop. Give the pins their own non-shared tmpfs instead of creating a
    # newer service mount namespace or publishing them below shared /run.
    systemd.mounts = [
      {
        description = "Tascarrel transient runtime filesystem";
        what = "tmpfs";
        where = "/run/tascarrel";
        type = "tmpfs";
        # runc resolves the transient rootfs path after joining the pod user
        # namespace. Search-only access permits traversal without exposing a
        # directory listing; bundle files and namespace pins remain private.
        options = "mode=0711,nodev,nosuid";
        wantedBy = [ "local-fs.target" ];
        before = [ "tascarrel-runtime-mount-private.service" ];
      }
    ];

    systemd.services.tascarrel-runtime-mount-private = {
      description = "Make the Tascarrel runtime mount propagation-private";
      requiredBy = [ "tascarrel-guest.service" ];
      requires = [ "run-tascarrel.mount" ];
      after = [ "run-tascarrel.mount" ];
      before = [ "tascarrel-guest.service" ];
      serviceConfig = {
        Type = "oneshot";
        ExecStart = "${lib.getExe' pkgs.util-linux "mount"} --make-private /run/tascarrel";
        RemainAfterExit = true;
      };
    };

    # A development host can expose locally built Linux binaries through the
    # portable shared-directory backend. Bind mounts preserve the immutable
    # image and persistent Nix stores while making both guestd and the pod PID
    # 1 observe the development executables for this boot only.
    systemd.services.tascarrel-local-binaries = {
      description = "Activate locally built Tascarrel guest binaries";
      unitConfig.ConditionKernelCommandLine = "tascarrel.local-binaries=1";
      requires = [
        "run-tascarrel.mount"
        "tascarrel-pod-nix-store.service"
      ];
      after = [
        "run-tascarrel.mount"
        "tascarrel-pod-nix-store.service"
      ];
      before = [ "tascarrel-guest.service" ];
      path = [
        pkgs.coreutils
        pkgs.util-linux
      ];
      script = ''
        set -euo pipefail
        install -d -m 0755 ${lib.escapeShellArg localBinariesDirectory}
        if ! mount -t virtiofs -o ro,nodev,nosuid \
          ${lib.escapeShellArg localBinariesMountTag} \
          ${lib.escapeShellArg localBinariesDirectory}; then
          mount -t 9p \
            -o trans=virtio,version=9p2000.L,ro,nodev,nosuid \
            ${lib.escapeShellArg localBinariesMountTag} \
            ${lib.escapeShellArg localBinariesDirectory}
        fi

        bind_binary() {
          local name="$1"
          local target="$2"
          local source=${lib.escapeShellArg localBinariesDirectory}/"$name"
          if [[ -L "$source" || ! -f "$source" || ! -x "$source" ]]; then
            echo "local Tascarrel binary is not a real executable file: $source" >&2
            exit 1
          fi
          if [[ ! -f "$target" ]]; then
            echo "image Tascarrel binary target does not exist: $target" >&2
            exit 1
          fi
          mount --bind "$source" "$target"
          mount -o remount,bind,ro,nodev,nosuid "$target"
        }

        bind_binary tascarrel-guest ${lib.escapeShellArg guestBinary}
        bind_binary tascarrel-podd ${lib.escapeShellArg poddBinary}
        bind_binary tascarrel-podd ${lib.escapeShellArg persistentPoddBinary}
        bind_binary podctl ${lib.escapeShellArg podctlBinary}
        bind_binary podctl ${lib.escapeShellArg persistentPodctlBinary}
        bind_binary tasci-exec ${lib.escapeShellArg tasciBinary}
        echo "tascarrel: using local guest binaries from ${localBinariesDirectory}"
      '';
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
        StandardOutput = "journal+console";
        StandardError = "journal+console";
      };
    };

    systemd.paths.tascarrel-guest = {
      description = "Watch for the Tascarrel virtio-serial port";
      wantedBy = [ "multi-user.target" ];
      pathConfig = {
        PathExists = cfg.device;
        Unit = "tascarrel-guest.service";
      };
    };

    systemd.services.tascarrel-nested-kvm = {
      description = "Load nested KVM support when the outer VM exposes it";
      before = [ "tascarrel-guest.service" ];
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
      };
      script =
        if system == "x86_64-linux" then
          ''
            if [[ ! -e /dev/kvm ]]; then
              if ${lib.getExe pkgs.gnugrep} -qw vmx /proc/cpuinfo; then
                ${lib.getExe' pkgs.kmod "modprobe"} kvm-intel \
                  || echo "tascarrel: nested Intel KVM is unavailable" >&2
              elif ${lib.getExe pkgs.gnugrep} -qw svm /proc/cpuinfo; then
                ${lib.getExe' pkgs.kmod "modprobe"} kvm-amd \
                  || echo "tascarrel: nested AMD KVM is unavailable" >&2
              fi
            fi
          ''
        else
          ''
            if [[ ! -e /dev/kvm ]]; then
              ${lib.getExe' pkgs.kmod "modprobe"} kvm \
                || echo "tascarrel: nested KVM is unavailable" >&2
            fi
          '';
    };

    systemd.services.tascarrel-guest = {
      description = "Tascarrel guest daemon";
      wants = [ "tascarrel-nested-kvm.service" ];
      after = [
        "systemd-udevd.service"
        "systemd-tmpfiles-setup.service"
        "nftables.service"
        "tascarrel-pod-nix-daemon.socket"
        "tascarrel-pod-nix-store.service"
        "tascarrel-local-binaries.service"
        "tascarrel-nested-kvm.service"
      ];
      requires = [
        "nftables.service"
        "tascarrel-pod-nix-daemon.socket"
        "tascarrel-pod-nix-store.service"
        "tascarrel-local-binaries.service"
      ];
      path = [
        pkgs.btrfs-progs
        pkgs.buildkit
        pkgs.coreutils
        pkgs.docker
        pkgs.fuse-overlayfs
        pkgs.iproute2
        pkgs.nftables
        pkgs.podman
        pkgs.runc
        pkgs.shadow
        pkgs.umoci
        pkgs.util-linux
      ];
      unitConfig = {
        RequiresMountsFor = [
          "/run/tascarrel"
        ]
        ++ lib.optionals (cfg.dataDevice != null) [ stateDirectory ];
        # NixOS reloads nftables from a complete ruleset. Restart guestd with
        # it so a firewall reload can never leave live pod veths without the
        # Tascarrel fail-closed table.
        PartOf = [ "nftables.service" ];
      };
      environment = {
        TASCARREL_GUEST_DEVICE = cfg.device;
        TASCARREL_GUEST_IP = lib.getExe' pkgs.iproute2 "ip";
        TASCARREL_GUEST_LOGIN_SHELL = lib.getExe pkgs.bashInteractive;
        TASCARREL_GUEST_TERMINAL_SHELL = tascarrelTerminalShell;
        TASCARREL_GUEST_NFT = lib.getExe' pkgs.nftables "nft";
        TASCARREL_GUEST_NIX = lib.getExe config.nix.package;
        TASCARREL_GUEST_POD_NIX_DAEMON_SOCKET_DIR = podNixSocketDirectory;
        TASCARREL_GUEST_RUNTIME_DIR = "/run/tascarrel";
        TASCARREL_GUEST_STATE_DIR = stateDirectory;
        TASCARREL_GUEST_BTRFS = lib.getExe' pkgs.btrfs-progs "btrfs";
        TASCARREL_GUEST_BUILDCTL = lib.getExe' pkgs.buildkit "buildctl";
        TASCARREL_GUEST_BUILDKITD = lib.getExe' pkgs.buildkit "buildkitd";
        TASCARREL_GUEST_CP = lib.getExe' pkgs.coreutils "cp";
        TASCARREL_GUEST_CODE_SERVER = toString cfg.codeServerPackage;
        TASCARREL_GUEST_DOCKERD = lib.getExe' pkgs.docker "dockerd";
        TASCARREL_GUEST_DOCKER = lib.getExe' pkgs.docker "docker";
        TASCARREL_GUEST_PODMAN = lib.getExe pkgs.podman;
        TASCARREL_GUEST_NEWUIDMAP = lib.getExe' pkgs.shadow "newuidmap";
        TASCARREL_GUEST_NEWGIDMAP = lib.getExe' pkgs.shadow "newgidmap";
        TASCARREL_GUEST_GIT = lib.getExe pkgs.git;
        TASCARREL_GUEST_HARNESS_USER = "tascarrel-harness";
        TASCARREL_GUEST_MOUNT = lib.getExe' pkgs.util-linux "mount";
        TASCARREL_GUEST_NSENTER = lib.getExe' pkgs.util-linux "nsenter";
        TASCARREL_GUEST_PODD = poddBinary;
        TASCARREL_GUEST_PODCTL = podctlBinary;
        TASCARREL_GUEST_TASCI = tasciBinary;
        TASCARREL_GUEST_RUNC = lib.getExe' pkgs.runc "runc";
        TASCARREL_GUEST_TAR = lib.getExe' pkgs.gnutar "tar";
        TASCARREL_GUEST_UMOCI = lib.getExe' pkgs.umoci "umoci";
        TASCARREL_GUEST_UMOUNT = lib.getExe' pkgs.util-linux "umount";
        TASCARREL_GUEST_UNSHARE = lib.getExe' pkgs.util-linux "unshare";
      };
      startLimitIntervalSec = 0;
      serviceConfig = {
        Type = "simple";
        ExecStart = pkgs.writeShellScript "tascarrel-guest-start" ''
          guest_instance_id=
          guest_instance_id_seen=
          read -r -a kernel_parameters < /proc/cmdline
          for kernel_parameter in "''${kernel_parameters[@]}"; do
            case "$kernel_parameter" in
              tascarrel.guest-instance-id=*)
                if [[ -n "$guest_instance_id_seen" ]]; then
                  echo "tascarrel: kernel command line contains multiple guest instance IDs" >&2
                  exit 1
                fi
                guest_instance_id_seen=1
                guest_instance_id="''${kernel_parameter#tascarrel.guest-instance-id=}"
                ;;
            esac
          done
          if [[ -z "$guest_instance_id" ]]; then
            echo "tascarrel: kernel command line contains no non-empty guest instance ID" >&2
            exit 1
          fi
          export TASCARREL_GUEST_INSTANCE_ID="$guest_instance_id"
          exec ${guestBinary} ${lib.escapeShellArgs cfg.extraArgs}
        '';
        User = "root";
        Group = "root";
        # runc seals and re-executes itself through /proc/self/fd during its
        # user-namespace bootstrap; a 0077 service umask makes that bootstrap
        # fail with EACCES. Tascarrel state uses explicit private modes, so the
        # conventional executable-safe umask is appropriate here.
        UMask = "0022";
        Restart = "always";
        RestartSec = "1s";
        StandardOutput = "journal+console";
        StandardError = "journal+console";
        # Do not use StateDirectory here. systemd may recursively repair its
        # ownership when it differs from User=/Group=, which is incompatible
        # with Tascarrel's immutable backing ownership and mandatory idmapped
        # mounts. The Btrfs mount and guestd validate this path fail-closed.
        Delegate = true;
        TasksMax = "infinity";
        LimitNOFILE = 1048576;
      };
    };

    # Match the conventional in-pod `develop` identity so provider-enforced
    # owner-only credential files remain usable in both execution contexts.
    users.groups.tascarrel-harness.gid = 1000;
    users.users.tascarrel-harness = {
      isSystemUser = true;
      group = "tascarrel-harness";
      uid = 1000;
      description = "Unprivileged Tascarrel chat harness account";
    };
  };
}
