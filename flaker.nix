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
      "npins-0.3.1" = "sha256-WQxAZYj7wFA46X2L0IuA3syzr8CopmoXA8eePyhnE0o=";
      "nix-compat-0.1.0" = "sha256-4K3J3slOOZYZCo3OF66yQ2QkBlF6uMQXQirXoYznwbQ=";
    };
  };

  inherit src;

  doCheck = false;
  meta.mainProgram = cargoToml.package.name;
}
