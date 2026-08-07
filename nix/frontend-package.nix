{
  lib,
  stdenvNoCC,
  nodejs,
  pnpm_11,
  fetchPnpmDeps,
  pnpmConfigHook,
  terminalNerdFont,
  zstd,
}:

let
  frontendRoot = toString ../frontend;
  frontendSource = lib.cleanSourceWith {
    name = "tascarrel-frontend-source";
    src = ../frontend;
    filter =
      path: _type:
      let
        relative = lib.removePrefix "${frontendRoot}/" (toString path);
        topLevel = lib.head (lib.splitString "/" relative);
      in
      toString path == frontendRoot
      || !builtins.elem topLevel [
        "dist"
        "node_modules"
      ];
  };
  pnpmDeps =
    (fetchPnpmDeps {
      pname = "tascarrel-frontend";
      version = "0.0.0";
      src = frontendSource;
      pnpm = pnpm_11;
      fetcherVersion = 4;
      nativeBuildInputs = [ zstd.bin ];
      hash = "sha256-IgtA1NxgXlNbymooTbUjlJwBIMuNLkTiBPhmjQIfVm4=";
    }).overrideAttrs
      (old: {
        # Avoid a nixpkgs 26.11 split-output issue where `tar --zstd` resolves the
        # development output instead of the zstd executable.
        fixupPhase =
          lib.replaceString "--zstd -cf $out/pnpm-store.tar.zst ."
            "-cf - . | ${zstd.bin}/bin/zstd --threads=1 --quiet -o $out/pnpm-store.tar.zst"
            old.fixupPhase;
      });
in
stdenvNoCC.mkDerivation (finalAttrs: {
  pname = "tascarrel-frontend";
  version = "0.0.0";
  src = frontendSource;

  inherit pnpmDeps;
  TASCARREL_TERMINAL_FONT_PATH = terminalNerdFont;

  nativeBuildInputs = [
    nodejs
    pnpm_11
    pnpmConfigHook
  ];

  buildPhase = ''
    runHook preBuild
    pnpm run build
    runHook postBuild
  '';

  installPhase = ''
    runHook preInstall
    mkdir -p "$out"
    cp -R dist/. "$out/"
    runHook postInstall
  '';

  meta = {
    description = "Tascarrel browser interface";
    license = with lib.licenses; [
      asl20
      mit
      ofl
    ];
    platforms = lib.platforms.all;
  };
})
