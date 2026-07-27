{
  config,
  lib,
  ...
}:

let
  cfg = config.services.tascarrel;
in
{
  options.services.tascarrel = {
    enable = lib.mkEnableOption "the Tascarrel per-user host daemon";

    package = lib.mkOption {
      type = lib.types.package;
      description = "Tascarrel distribution installed into the user profile and run by the service.";
    };

    runtimePackages = lib.mkOption {
      type = lib.types.listOf lib.types.package;
      default = [ ];
      description = ''
        Packages providing host runtime dependencies such as Git, OpenSSH, SOPS, and QEMU.
      '';
    };

    home = lib.mkOption {
      type = lib.types.strMatching "^/.*";
      default = "${config.home.homeDirectory}/.tascarrel";
      defaultText = lib.literalExpression ''"${config.home.homeDirectory}/.tascarrel"'';
      description = "Absolute directory containing Tascarrel configuration and state.";
    };

    webAddress = lib.mkOption {
      type = lib.types.str;
      default = "127.0.0.1:8272";
      description = "Loopback address for the Tascarrel browser interface and API.";
    };

    autoStart = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = "Whether to start Tascarrel with the systemd user manager.";
    };
  };

  config = lib.mkIf cfg.enable {
    home.packages = [ cfg.package ];
    home.sessionVariables.TASCARREL_HOME = cfg.home;

    systemd.user.services.tascarrel = {
      Unit.Description = "Tascarrel host daemon";

      Service = {
        Type = "simple";
        ExecStart = lib.getExe cfg.package;
        Environment = [
          "TASCARREL_HOME=${cfg.home}"
          "TASCARREL_WEB_ADDRESS=${cfg.webAddress}"
        ]
        ++ lib.optional (cfg.runtimePackages != [ ]) "PATH=${lib.makeBinPath cfg.runtimePackages}";
        UMask = "0077";
        Restart = "on-failure";
        RestartSec = "3s";
      };

      Install.WantedBy = lib.optionals cfg.autoStart [ "default.target" ];
    };
  };
}
