{
  config,
  lib,
  modulesPath,
  pkgs,
  tascarrelGuestPackage,
  tascarrelPodctlPackage,
  tascarrelPoddPackage,
  tascarrelTasciPackage,
  ...
}:

let
  system = pkgs.stdenv.hostPlatform.system;
  architecture = lib.removeSuffix "-linux" system;
  console = if system == "aarch64-linux" then "ttyAMA0" else "ttyS0";
  storeLabel = "tascarrel";
  storeRegistration = ".tascarrel-store-registration";
  sourceStoreNixConfig = "build-users-group =";
  storeClosure = pkgs.closureInfo {
    rootPaths = [ config.system.build.toplevel ];
  };
in
{
  imports = [
    "${modulesPath}/profiles/minimal.nix"
    "${modulesPath}/profiles/qemu-guest.nix"
    "${modulesPath}/image/file-options.nix"
  ];

  assertions = [
    {
      assertion = builtins.elem system [
        "x86_64-linux"
        "aarch64-linux"
      ];
      message = "Tascarrel images support only x86_64-linux and aarch64-linux";
    }
  ];

  image = {
    baseName = "tascarrel-${architecture}";
    extension = "erofs";
  };

  system.build.image =
    pkgs.runCommand "tascarrel-${architecture}-erofs"
      {
        nativeBuildInputs = [
          pkgs.erofs-utils
          pkgs.gnutar
        ];
      }
      ''
        mkdir -p "$out" .links
        cp ${storeClosure}/registration ${storeRegistration}
        tar --create \
          --absolute-names \
          --verbatim-files-from \
          --transform 'flags=rSh;s|/nix/store/||' \
          --transform 'flags=rSh;s|~nix~case~hack~[[:digit:]]\+||g' \
          --files-from ${storeClosure}/store-paths \
          --directory . \
          .links \
          ${storeRegistration} \
          | mkfs.erofs \
            --quiet \
            --force-uid=0 \
            --force-gid=0 \
            -L ${storeLabel} \
            -U 776f726b-626f-4778-9379-7374656d0001 \
            -T 0 \
            --hard-dereference \
            --tar=f \
            "$out/${config.image.filePath}"
      '';

  services.tascarrel-guest = {
    enable = true;
    package = tascarrelGuestPackage;
    podctlPackage = tascarrelPodctlPackage;
    poddPackage = tascarrelPoddPackage;
    tasciPackage = tascarrelTasciPackage;
    portName = "tascarrel-control";
    networkIsolation = true;
    dataDevice = "/dev/disk/by-id/virtio-tascarrel-data";
    autoFormatDataDevice = true;
  };

  systemd.services.register-nix-paths = {
    description = "Register the immutable Tascarrel Nix store";
    environment = {
      NIX_CONFIG = sourceStoreNixConfig;
      NIX_REMOTE = "local";
    };
    requires = [ "systemd-tmpfiles-setup.service" ];
    unitConfig = {
      DefaultDependencies = false;
      RequiresMountsFor = "/nix/.ro-store";
    };
    wantedBy = [ "sysinit.target" ];
    before = [
      "sysinit.target"
      "shutdown.target"
      "nix-daemon.socket"
      "nix-daemon.service"
    ];
    after = [
      "local-fs.target"
      "systemd-tmpfiles-setup.service"
    ];
    conflicts = [ "shutdown.target" ];
    restartIfChanged = false;
    serviceConfig = {
      Type = "oneshot";
      RemainAfterExit = true;
      StandardOutput = "journal+console";
      StandardError = "journal+console";
    };
    script = ''
      ${lib.getExe' config.nix.package "nix-store"} --load-db \
        < /nix/.ro-store/${storeRegistration}
    '';
  };

  # The VM daemon only reads the EROFS source store. Build users would make
  # local-store initialization change the immutable store root's ownership.
  systemd.services.nix-daemon.environment.NIX_CONFIG = sourceStoreNixConfig;

  boot = {
    initrd.systemd.enable = true;
    kernelParams = [
      "console=${console},115200n8"
      "panic=1"
      "boot.panic_on_fail"
    ];
    loader.grub.enable = false;
    loader.systemd-boot.enable = false;
    supportedFilesystems = [ "erofs" ];
    tmp.useTmpfs = true;
  };

  fileSystems = {
    "/" = {
      device = "tmpfs";
      fsType = "tmpfs";
      options = [ "mode=0755" ];
    };
    "/nix/.ro-store" = {
      device = "/dev/disk/by-label/${storeLabel}";
      fsType = "erofs";
      neededForBoot = true;
      options = [ "ro" ];
    };
    "/nix/store" = {
      device = "/nix/.ro-store";
      fsType = "none";
      neededForBoot = true;
      options = [ "bind" ];
    };
  };

  system.etc.overlay = {
    enable = true;
    mutable = false;
  };
  services.userborn.enable = true;
  nix.enable = true;
  system.switch.enable = false;

  networking.hostName = "tascarrel";

  # This appliance is managed exclusively over the authenticated Tascarrel
  # channel. Accounts stay locked and no interactive login service is enabled.
  users = {
    mutableUsers = false;
    allowNoPasswordLogin = true;
    users.root.hashedPassword = "!";
  };
  services.openssh.enable = false;

  environment.systemPackages = [
    pkgs.bashInteractive
    pkgs.btrfs-progs
    pkgs.buildkit
    pkgs.fuse-overlayfs
    pkgs.runc
    pkgs.umoci
    pkgs.util-linux
  ];

  documentation.enable = false;
  programs.command-not-found.enable = false;
  security.sudo.enable = false;
  system.disableInstallerTools = true;

  system.stateVersion = "24.11";
}
