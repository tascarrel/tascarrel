{
  electron,
  fetchPnpmDeps,
  lib,
  makeWrapper,
  nodejs,
  pnpm_11,
  pnpmConfigHook,
  server,
  source,
  stdenvNoCC,
}:

let
  desktopRoot = toString "${source}/crates/apps/tascarrel-desktop";
  desktopMetadata = builtins.fromJSON (builtins.readFile "${desktopRoot}/package.json");
  desktopSource = lib.cleanSourceWith {
    name = "tascarrel-desktop-source";
    src = "${source}/crates/apps/tascarrel-desktop";
    filter =
      path: _type:
      let
        relative = lib.removePrefix "${desktopRoot}/" (toString path);
        topLevel = lib.head (lib.splitString "/" relative);
      in
      toString path == desktopRoot
      || !builtins.elem topLevel [
        "dist"
        "node_modules"
        "out"
      ];
  };
  pnpmDeps = fetchPnpmDeps {
    pname = "tascarrel-desktop";
    inherit (desktopMetadata) version;
    src = desktopSource;
    pnpm = pnpm_11;
    fetcherVersion = 4;
    hash = "sha256-1bSPXpt8CQuZpWCbdedqSDOKRgvndRJu6OGCVllDuME=";
  };
in
stdenvNoCC.mkDerivation {
  pname = "tascarrel-desktop";
  inherit (desktopMetadata) version;
  src = desktopSource;

  inherit pnpmDeps;

  nativeBuildInputs = [
    makeWrapper
    nodejs
    pnpm_11
    pnpmConfigHook
  ];

  buildPhase = ''
    runHook preBuild
    TASCARREL_WORKSPACE_ROOT=${source} pnpm run build
    runHook postBuild
  '';

  installPhase = ''
    runHook preInstall
    application="$out/share/tascarrel-desktop"
    mkdir -p "$application" "$out/bin"
    cp package.json "$application/"
    cp -R dist icons "$application/"
    makeWrapper ${lib.getExe electron} "$out/bin/tascarrel-desktop" \
      --add-flags "$application" \
      --set TASCARREL_DESKTOP_SERVER ${lib.getExe server}

    install -Dm644 icons/icon.svg \
      "$out/share/icons/hicolor/scalable/apps/tascarrel.svg"
    install -Dm644 /dev/stdin \
      "$out/share/applications/dev.tascarrel.Tascarrel.desktop" <<EOF
    [Desktop Entry]
    Type=Application
    Name=Tascarrel
    GenericName=Development Environment
    Comment=Local-first agentic development environment
    Exec=$out/bin/tascarrel-desktop
    Icon=tascarrel
    Categories=Development;
    Terminal=false
    StartupNotify=true
    StartupWMClass=Tascarrel
    EOF
    runHook postInstall
  '';

  meta = {
    description = "Tascarrel desktop shell";
    license = with lib.licenses; [
      asl20
      mit
    ];
    mainProgram = "tascarrel-desktop";
    platforms = lib.platforms.linux;
  };
}
