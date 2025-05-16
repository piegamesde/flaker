let
  pins = import ./npins;
  pkgs = import pins.nixpkgs { };
  lib = pkgs.lib;
  # TODO
  nixA = null;
  nixB = null;
in
rec {
  flaker = pkgs.callPackage ./flaker.nix { };

  sources = builtins.scopedImport {
    builtins = builtins // {
      fromJSON = _: builtins.fromJSON (builtins.readFile ./test.json);
      fetchTarball = { url, sha256 }: pkgs.fetchzip { inherit url sha256; };
      fetchurl = { url, sha256 }: pkgs.fetchurl { inherit url sha256; };
      fetchGit =
        {
          url,
          submodules,
          rev,
          name,
          narHash,
        }:
        pkgs.fetchgit {
          inherit url rev name;
          fetchSubmodules = submodules;
          hash = narHash;
        };
    };
  } ./npins/default.nix;

  reports = lib.mapAttrs (
    name: pin:
    pkgs.stdenv.mkDerivation {
      inherit name;
      src = pin.outPath;
      buildInputs = [
        flaker
        nixA
        nixB
      ];
      buildPhase = ''
        flaker nix-parse pin ${nixA} ${nixB}
      '';
      installPhase = "cp report.json $out";
    }
  ) sources;

  reports-combined = pkgs.stdenv.mkDerivation {
    # ./pin1, ./pin2, ...
    srcs = builtins.attrValues reports;
    sourceRoot = ".";

    buildPhase = ''
      jq "[.]" * -o report-combined.json
    '';
    installPhase = "cp report-combined.json $out";
  };
}
