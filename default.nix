# Usage:
# nix-build -A reports-combined --keep-going \
#     --arg nixA '{ url = "file://path/to/lix"; rev = "0000000000000000000000000000000000000000";}' \
#     --arg nixB '{ url = "file://path/to/lix"; ref = "2.94.0";}'
# or
# nix-build -A reports.\"https://github.com/nixos/nixpkgs\" --keep-going \
#     --arg nixA /path/to/lix/repo \
#     --arg nixB '{ url = "file://path/to/lix"; ref = "2.94.0";}'
#
# nix-build -A lixA \
#     --arg nixA '{ url = "file://path/to/lix"; rev = "0000000000000000000000000000000000000000";}'
#
# nix-build -A sources.\"https://github.com/nixos/nixpkgs\"
let
  pins = import ./npins;
  pkgs = import pins.nixpkgs { };
  lib = pkgs.lib;
in
rec {
  flaker = pkgs.callPackage ./flaker.nix { };

  # Call all pins with a Nixpkgs to make them proper derivations
  sources = lib.mapAttrs (_: pin: pin { inherit pkgs; }) (import ./npins { input = ./index.json; });

  lixA = { nixA, ... }: (import (builtins.fetchGit nixA)).default;
  lixB = { nixB, ... }: (import (builtins.fetchGit nixB)).default;

  reports =
    {
      nixA,
      nixB,
      lixACached ? lixA nixArgs,
      lixBCached ? lixB nixArgs,
    }@nixArgs:
    lib.mapAttrs (
      name: pin:
      pkgs.stdenvNoCC.mkDerivation {
        inherit name;
        src = pin.outPath;
        buildInputs = [
          flaker
          lixACached
          lixBCached
          pkgs.jq
        ];
        dontConfigure = true;
        buildPhase = ''
          flaker diff . ${lixACached}/bin/nix ${lixBCached}/bin/nix
        '';
        installPhase = ''
          mkdir $out
          cp report.json $out/report-$(echo ${lib.escapeShellArg name} | jq --raw-input --raw-output '@uri').json
        '';
        dontFixup = true;
      }
    ) sources;

  reports-combined =
    {
      nixA,
      nixB,
      lixACached ? lixA nixArgs,
      lixBCached ? lixB nixArgs,
    }@nixArgs:
    pkgs.symlinkJoin {
      name = "reports-combined";
      paths = builtins.attrValues (reports nixArgs);
    };
}
