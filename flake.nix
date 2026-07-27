{
  description = "Tascarrel VM images and Rust workspace";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    guest-nixpkgs.url = "github:NixOS/nixpkgs/241313f4e8e508cb9b13278c2b0fa25b9ca27163";
    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    home-manager = {
      url = "github:nix-community/home-manager";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    nix-appimage = {
      url = "github:ralismark/nix-appimage";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };
  outputs =
    {
      fenix,
      guest-nixpkgs,
      home-manager,
      nix-appimage,
      self,
      nixpkgs,
    }:
    let
      inherit (nixpkgs) lib;

      linuxSystems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      supportedSystems = linuxSystems ++ [
        "aarch64-darwin"
      ];

      forAllSystems = lib.genAttrs supportedSystems;
      buildRevision = self.rev or self.dirtyRev or "development";
      pkgsFor = system: import nixpkgs { inherit system; };
      # Nettle defaults to small-GOT -fpic, which overflows in AArch64 QEMU's static PIE.
      guestNixpkgsOverlays = [
        (final: previous: {
          nettle =
            if final.stdenv.hostPlatform.isAarch64 && final.stdenv.hostPlatform.isStatic then
              previous.nettle.overrideAttrs (old: {
                env = (old.env or { }) // {
                  CCPIC = "-fPIC";
                };
              })
            else
              previous.nettle;
        })
      ];
      guestPkgsFor =
        system:
        import guest-nixpkgs {
          inherit system;
          overlays = guestNixpkgsOverlays;
        };
      nightlyToolchainFor =
        system:
        fenix.packages.${system}.latest.withComponents [
          "cargo"
          "clippy"
          "rustc"
          "rustfmt"
        ];
      stableRustPlatformFor =
        system: pkgs:
        pkgs.makeRustPlatform {
          cargo = fenix.packages.${system}.stable.cargo;
          rustc = fenix.packages.${system}.stable.rustc;
        };
      stableRustPlatformForTarget =
        system: pkgs: target:
        let
          fenixPackages = fenix.packages.${system};
          toolchain = fenixPackages.combine [
            fenixPackages.stable.cargo
            fenixPackages.stable.rustc
            fenixPackages.targets.${target}.stable.rust-std
          ];
        in
        pkgs.makeRustPlatform {
          cargo = toolchain;
          rustc = toolchain;
        };

      workspacePackageWith =
        system: pkgs: arguments:
        pkgs.callPackage ./nix/workspace-package.nix (
          {
            inherit buildRevision;
            rustPlatform = stableRustPlatformFor system pkgs;
            source = self;
          }
          // arguments
        );

      workspacePackageFor = system: workspacePackageWith system (pkgsFor system);
      guestWorkspacePackageFor = system: workspacePackageWith system (guestPkgsFor system);

      guestPackageFor =
        system:
        guestWorkspacePackageFor system {
          cargoPackage = "tascarrel-guest";
          binaryName = "tascarrel-guest";
          description = "Tascarrel daemon running inside guest VMs";
          nativeCheckInputs = [
            (guestPkgsFor system).git
            (guestPkgsFor system).util-linux
          ];
        };

      poddPackageFor =
        system:
        guestWorkspacePackageFor system {
          cargoPackage = "tascarrel-podd";
          binaryName = "tascarrel-podd";
          description = "Tascarrel pod PID 1 and nested-service supervisor";
        };

      podctlPackageFor =
        system:
        guestWorkspacePackageFor system {
          cargoPackage = "tascarrel-podctl";
          binaryName = "podctl";
          description = "Tascarrel in-pod control client";
        };

      tasciExecPackageFor =
        system:
        guestWorkspacePackageFor system {
          cargoPackage = "tasci-exec";
          binaryName = "tasci-exec";
          description = "Tasci coding-agent harness";
        };

      cliPackageFor =
        system:
        workspacePackageFor system {
          cargoPackage = "tascarrel-cli";
          binaryName = "tascarrelctl";
          description = "Administrative command-line client for Tascarrel";
        };

      terminalNerdFontFor =
        pkgs:
        let
          font = pkgs.runCommand "tascarrel-terminal-nerd-font" { nativeBuildInputs = [ pkgs.woff2 ]; } ''
            mkdir -p "$out"
            cp \
              "${pkgs.nerd-fonts.symbols-only}/share/fonts/truetype/NerdFonts/Symbols/SymbolsNerdFontMono-Regular.ttf" \
              "$out/SymbolsNerdFontMono-Regular.ttf"
            woff2_compress "$out/SymbolsNerdFontMono-Regular.ttf"
            rm "$out/SymbolsNerdFontMono-Regular.ttf"
          '';
        in
        "${font}/SymbolsNerdFontMono-Regular.woff2";
      frontendPackageFor =
        system:
        let
          pkgs = pkgsFor system;
        in
        pkgs.callPackage ./nix/frontend-package.nix {
          terminalNerdFont = terminalNerdFontFor pkgs;
        };
      qemuPackageFor =
        system:
        let
          pkgs = pkgsFor system;
        in
        if pkgs.stdenv.hostPlatform.isDarwin then pkgs.qemu else pkgs.qemu_kvm;

      guestPayloadFor =
        buildSystem: guestSystem:
        let
          pkgs = pkgsFor buildSystem;
          nixos = nixosSystems.${guestSystem};
          image = nixos.config.system.build.image;
          imageFile = "${image}/${nixos.config.image.filePath}";
          kernelFile = "${nixos.config.system.build.kernel}/${nixos.config.system.boot.loader.kernelFile}";
          initrdFile = "${nixos.config.system.build.initialRamdisk}/initrd";
          kernelAppend = lib.concatStringsSep " " (
            [
              "init=${nixos.config.system.build.toplevel}/init"
            ]
            ++ nixos.config.boot.kernelParams
          );
          architecture = lib.removeSuffix "-linux" guestSystem;
          frontend = frontendPackageFor buildSystem;
        in
        pkgs.runCommand "tascarrel-${architecture}-guest-payload"
          {
            nativeBuildInputs = [
              pkgs.gnutar
              pkgs.xz
            ];
          }
          ''
            mkdir -p "$out"
            payload=$(mktemp -d)
            payload_archive="$out/payload.tar.xz"
            cp "${imageFile}" "$payload/system.erofs"
            cp "${kernelFile}" "$payload/kernel"
            cp "${initrdFile}" "$payload/initrd"
            printf '%s\n' ${lib.escapeShellArg kernelAppend} > "$payload/kernel-append"
            mkdir "$payload/ui"
            cp -R "${frontend}/." "$payload/ui/"

            tar -C "$payload" -cf - . \
              | xz --compress --stdout --threads=0 --check=crc64 > "$payload_archive"
            sha256sum "$payload_archive" | cut -d ' ' -f 1 > "$out/payload.sha256"
            stat --format '%s' "$payload_archive" > "$out/payload.size"
            printf '%s\n' '${architecture}' > "$out/architecture"

            archive_contents=$(tar -tJf "$payload_archive")
            for required_path in \
              ./system.erofs \
              ./kernel \
              ./initrd \
              ./kernel-append \
              ./ui/; do
              grep -Fx "$required_path" <<< "$archive_contents"
            done
          '';

      distributionPackageFor =
        system:
        let
          pkgs = pkgsFor system;
          buildPkgs = if pkgs.stdenv.hostPlatform.isLinux then pkgs.pkgsStatic else pkgs;
          rustPlatform =
            if pkgs.stdenv.hostPlatform.isLinux then
              stableRustPlatformForTarget system buildPkgs
                "${lib.removeSuffix "-linux" system}-unknown-linux-musl"
            else
              stableRustPlatformFor system buildPkgs;
        in
        buildPkgs.callPackage ./nix/workspace-package.nix {
          inherit buildRevision rustPlatform;
          source = self;
          cargoPackage = "tascarrel-cli";
          binaryName = "tascarrel";
          description = "Self-contained Tascarrel server and guest image";
          embeddedPayload = guestPayloadFor system (guestSystemFor system);
          doCheck = false;
        };

      desktopPackageFor =
        system:
        let
          pkgs = pkgsFor system;
          server = distributionPackageFor system;
        in
        pkgs.callPackage ./nix/desktop-package.nix {
          electron = pkgs.electron_42;
          inherit server;
          source = self;
        };

      mkTascarrelNixosSystem =
        {
          system,
          guestPackage ? guestPackageFor system,
          extraModules ? [ ],
        }:
        guest-nixpkgs.lib.nixosSystem {
          specialArgs = {
            tascarrelGuestPackage = guestPackage;
            tascarrelPodctlPackage = podctlPackageFor system;
            tascarrelPoddPackage = poddPackageFor system;
            tascarrelTasciPackage = tasciExecPackageFor system;
          };
          modules = [
            {
              nixpkgs = {
                hostPlatform = system;
                overlays = guestNixpkgsOverlays;
              };
            }
            ./nix/modules/tascarrel-guest.nix
            ./nix/image.nix
          ]
          ++ extraModules;
        };

      nixosSystems = lib.genAttrs linuxSystems (system: mkTascarrelNixosSystem { inherit system; });
      guestSystemFor =
        system:
        if lib.hasSuffix "-darwin" system then
          lib.replaceStrings [ "-darwin" ] [ "-linux" ] system
        else
          system;
    in
    {
      lib = {
        inherit supportedSystems linuxSystems mkTascarrelNixosSystem;
        virtioPortName = "tascarrel-control";
      };

      homeManagerModules = {
        default = self.homeManagerModules.tascarrel;
        tascarrel =
          { lib, pkgs, ... }:
          {
            imports = [ ./nix/modules/tascarrel.nix ];
            services.tascarrel.package =
              lib.mkDefault
                self.packages.${pkgs.stdenv.hostPlatform.system}.tascarrel;
            services.tascarrel.runtimePackages = lib.mkDefault [
              pkgs.git
              pkgs.openssh
              pkgs.sops
              (qemuPackageFor pkgs.stdenv.hostPlatform.system)
            ];
          };
      };

      nixosModules = {
        default = self.nixosModules.tascarrel-guest;
        tascarrel-guest = import ./nix/modules/tascarrel-guest.nix;
      };

      nixosConfigurations = {
        tascarrel-x86_64-linux = nixosSystems.x86_64-linux;
        tascarrel-aarch64-linux = nixosSystems.aarch64-linux;
      };

      packages = forAllSystems (
        system:
        let
          distribution = distributionPackageFor system;
          guestPayload = guestPayloadFor system (guestSystemFor system);
        in
        {
          default = distribution;
          vm-image = nixosSystems.${guestSystemFor system}.config.system.build.image;
          embedded-payload = guestPayload;
          guest-payload = guestPayload;
          web-ui = frontendPackageFor system;
          tascarrel = distribution;
          tascarrel-cli = cliPackageFor system;
          tascarrel-distribution = distribution;
        }
        // lib.optionalAttrs (lib.elem system linuxSystems) {
          tascarrel-desktop = desktopPackageFor system;
          tascarrel-desktop-appimage = nix-appimage.lib.${system}.mkAppImage {
            program = lib.getExe (desktopPackageFor system);
            pname = "tascarrel-desktop";
            name = "tascarrel-desktop-${system}.AppImage";
          };
          tascarrel-guest = guestPackageFor system;
          tascarrel-podctl = podctlPackageFor system;
          tascarrel-podd = poddPackageFor system;
          tasci-exec = tasciExecPackageFor system;
        }
        // lib.optionalAttrs (system == "x86_64-linux") {
          guest-payload-aarch64-linux = guestPayloadFor system "aarch64-linux";
          guest-payload-x86_64-linux = guestPayloadFor system "x86_64-linux";
        }
      );

      apps = forAllSystems (
        system:
        let
          tascarrel = self.packages.${system}.tascarrel;
        in
        {
          default = self.apps.${system}.tascarrel;
          host = {
            type = "app";
            program = lib.getExe tascarrel;
            meta.description = "Run the Tascarrel host daemon with its embedded payload";
          };
          tascarrel = {
            type = "app";
            program = lib.getExe tascarrel;
            meta.description = "Run Tascarrel with its embedded guest payload";
          };
          distribution = {
            type = "app";
            program = lib.getExe tascarrel;
            meta.description = "Run the Tascarrel distribution with an embedded guest image";
          };
        }
        // lib.optionalAttrs (lib.elem system linuxSystems) {
          desktop = {
            type = "app";
            program = lib.getExe self.packages.${system}.tascarrel-desktop;
            meta.description = "Open Tascarrel Desktop";
          };
          guest = {
            type = "app";
            program = lib.getExe' self.packages.${system}.tascarrel-guest "tascarrel-guest";
            meta.description = "Run the Tascarrel guest daemon";
          };
        }
      );

      checks = lib.genAttrs linuxSystems (
        system:
        let
          pkgs = pkgsFor system;
        in
        {
          inherit (self.packages.${system})
            web-ui
            tascarrel
            tascarrel-guest
            tascarrel-podctl
            tascarrel-podd
            tasci-exec
            ;

          guest-module = (guestPkgsFor system).callPackage ./nix/tests/guest-module.nix {
            guestModule = self.nixosModules.tascarrel-guest;
            portName = self.lib.virtioPortName;
          };

          home-manager-module = pkgs.callPackage ./nix/tests/home-manager-module.nix {
            homeManager = home-manager;
            homeManagerModule = self.homeManagerModules.tascarrel;
            runtimePackages = [
              pkgs.git
              pkgs.openssh
              pkgs.sops
              (qemuPackageFor system)
            ];
            tascarrelPackage = self.packages.${system}.tascarrel;
          };

          nix-format = pkgs.runCommand "tascarrel-nix-format" { nativeBuildInputs = [ pkgs.nixfmt ]; } ''
            mkdir source
            cp -r ${self}/flake.nix ${self}/nix source
            chmod -R u+w source
            find source -name '*.nix' -print0 | xargs -0 nixfmt --check
            touch "$out"
          '';

          rust-format =
            pkgs.runCommand "tascarrel-rust-format"
              {
                nativeBuildInputs = [ (nightlyToolchainFor system) ];
              }
              ''
                cargo fmt --manifest-path ${self}/Cargo.toml --all -- --check
                touch "$out"
              '';
        }
      );

      devShells = forAllSystems (
        system:
        let
          pkgs = pkgsFor system;
        in
        {
          default = pkgs.mkShell {
            TASCARREL_BUILD_REVISION = buildRevision;
            TASCARREL_TERMINAL_FONT_PATH = terminalNerdFontFor pkgs;
            packages =
              (with pkgs; [
                nixfmt
                pkg-config
                nodejs
                pnpm_11
                qemu
                rust-analyzer
              ])
              ++ lib.optionals pkgs.stdenv.hostPlatform.isLinux (
                with pkgs;
                [
                  openssl
                ]
              )
              ++ [ (nightlyToolchainFor system) ];
            RUSTFMT = lib.getExe' (nightlyToolchainFor system) "rustfmt";
          };
        }
      );

      formatter = forAllSystems (system: (pkgsFor system).nixfmt);
    };
}
