{
  lib,
  rustPlatform,
  source,
  buildRevision,
  cargoPackage,
  binaryName,
  description,
  pname ? binaryName,
  nativeCheckInputs ? [ ],
  nativeBuildInputs ? [ ],
  buildInputs ? [ ],
  embeddedPayload ? null,
  doCheck ? true,
  supportedPlatforms ? lib.platforms.unix,
}:

let
  sourceRoot = toString source;
  workspaceSource = lib.cleanSourceWith {
    name = "tascarrel-rust-source";
    src = source;
    filter =
      path: _type:
      let
        relative = lib.removePrefix "${sourceRoot}/" (toString path);
        topLevel = lib.head (lib.splitString "/" relative);
      in
      toString path == sourceRoot
      || topLevel == "crates"
      || builtins.elem relative [
        "Cargo.lock"
        "Cargo.toml"
        "LICENSE-APACHE"
        "LICENSE-MIT"
      ];
  };
in
rustPlatform.buildRustPackage {
  inherit pname;
  version = "0.1.0";

  src = workspaceSource;
  # Pass the lock contents directly so Nix-only source changes do not rebuild
  # Cargo dependencies or leave evaluation dependent on a filtered store path.
  cargoLock = {
    lockFileContents = builtins.readFile "${sourceRoot}/Cargo.lock";
    outputHashes = {
      "sidex-0.1.0" = "sha256-3EzJF9DF3IdzwQ5fjRAcz6OOTk0zS7ZPgSHDuCkQoMM=";
    };
  };

  cargoBuildFlags = [
    "--package"
    cargoPackage
  ];
  cargoTestFlags = [
    "--package"
    cargoPackage
  ];

  strictDeps = true;
  inherit
    buildInputs
    doCheck
    nativeBuildInputs
    nativeCheckInputs
    ;

  TASCARREL_EMBEDDED_PAYLOAD_DIR = lib.optionalString (embeddedPayload != null) (
    toString embeddedPayload
  );
  TASCARREL_BUILD_REVISION = buildRevision;

  passthru = {
    inherit binaryName cargoPackage;
  };

  meta = {
    inherit description;
    license = with lib.licenses; [
      asl20
      mit
    ];
    mainProgram = binaryName;
    platforms = supportedPlatforms;
  };
}
