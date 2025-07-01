let
  pins = import ./npins;
  pkgs = import pins.nixpkgs { };
  lib = pkgs.lib;
  # TODO
  nixA = null;
  nixB = null;
in
assert nixA != null;
assert nixB != null;
rec {
  flaker = pkgs.callPackage ./flaker.nix { };

  # Call all pins with a Nixpkgs to make them proper derivations
  sources = lib.mapAttrs (_: pin: pin { inherit pkgs; }) (import ./npins { input = ./test.json; });

  reports = lib.mapAttrs (
    name: pin:
    pkgs.stdenvNoCC.mkDerivation {
      inherit name;
      src = pin.outPath;
      buildInputs = [
        flaker
        nixA
        nixB
      ];
      buildPhase = ''
        flaker nix-parse . ${nixA}/bin/nix ${nixB}/bin/nix
      '';
      installPhase = "cp report.json $out";
    }
  ) sources;

  reports-combined = pkgs.linkFarm "report-combined" {
    # ./pin1, ./pin2, ...
    paths = reports;
  };
}
