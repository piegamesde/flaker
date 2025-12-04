# Usage:
# nix-build -A reports-combined --keep-going \
#     --arg nixA '{ url = "file://path/to/lix"; rev = "0000000000000000000000000000000000000000";}' \
#     --arg nixB '{ url = "file://path/to/lix"; ref = "2.94.0";}'
# or
# nix-build -A reports.\"https://github.com/nixos/nixpkgs\" --keep-going \
#     --arg nixA /path/to/lix/repo \
#     --arg nixB '{ url = "file://path/to/lix"; ref = "2.94.0";}'
let
  pins = import ./npins;
  pkgs = import pins.nixpkgs { };
  lib = pkgs.lib;
in
rec {
  flaker = pkgs.callPackage ./flaker.nix { };

  # Call all pins with a Nixpkgs to make them proper derivations
  sources = lib.mapAttrs (_: pin: pin { inherit pkgs; }) (import ./npins { input = ./test.json; });

  reports =
    { nixA, nixB }:
    let
      lixA = (import (builtins.fetchGit nixA)).default;
      lixB = (import (builtins.fetchGit nixB)).default;
    in
    lib.mapAttrs (
      name: pin:
      pkgs.stdenvNoCC.mkDerivation {
        inherit name;
        src = pin.outPath;
        buildInputs = [
          flaker
          lixA
          lixB
        ];
        buildPhase = ''
          flaker nix-parse . ${lixA}/bin/nix ${lixB}/bin/nix
        '';
        installPhase = "cp report.json $out";
      }
    ) sources;

  reports-combined = { nixA, nixB }@nixArgs: pkgs.linkFarm "report-combined" (reports nixArgs);
}
