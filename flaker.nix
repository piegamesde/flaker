{
  lib,
  rustPlatform,
  stdenv,
}:
let
  src = lib.fileset.toSource {
    root = ./.;
    fileset = lib.fileset.unions [
      ./src
      ./Cargo.lock
      ./Cargo.toml
    ];
  };
  cargoToml = builtins.fromTOML (builtins.readFile (src + "/Cargo.toml"));
in
rustPlatform.buildRustPackage {
  pname = cargoToml.package.name;
  version = cargoToml.package.version;
  cargoLock = {
    lockFile = src + "/Cargo.lock";

    outputHashes = {
      "npins-0.3.0" = "sha256-/FTE/lDICJnXr4JbxaA+9mwM0sSF5++/XaYR+S2pFdA=";
    };
  };

  inherit src;

  doCheck = false;
  meta.mainProgram = cargoToml.package.name;
}
