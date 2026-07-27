{
  homeManager,
  homeManagerModule,
  lib,
  pkgs,
  runtimePackages,
  tascarrelPackage,
}:

let
  evaluated = homeManager.lib.homeManagerConfiguration {
    inherit pkgs;
    modules = [
      homeManagerModule
      {
        home.username = "tascarrel-test";
        home.homeDirectory = "/home/tascarrel-test";
        home.stateVersion = "26.05";
        services.tascarrel.enable = true;
      }
    ];
  };
  evaluatedConfig = evaluated.config;
  service = evaluatedConfig.systemd.user.services.tascarrel;
in
assert evaluatedConfig.services.tascarrel.home == "/home/tascarrel-test/.tascarrel";
assert evaluatedConfig.services.tascarrel.runtimePackages == runtimePackages;
assert evaluatedConfig.home.sessionVariables.TASCARREL_HOME == "/home/tascarrel-test/.tascarrel";
assert builtins.elem tascarrelPackage evaluatedConfig.home.packages;
assert service.Service.ExecStart == [ (lib.getExe tascarrelPackage) ];
assert
  service.Service.Environment == [
    "TASCARREL_HOME=/home/tascarrel-test/.tascarrel"
    "TASCARREL_WEB_ADDRESS=127.0.0.1:8272"
    "PATH=${lib.makeBinPath runtimePackages}"
  ];
assert service.Install.WantedBy == [ "default.target" ];
pkgs.runCommand "tascarrel-home-manager-module" { } ''
  touch "$out"
''
