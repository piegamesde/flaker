{
  system ? builtins.currentSystem,
}:
let
  pins = import ./npins;
  pkgs = import pins.nixpkgs { inherit system; };
  inherit (pkgs) stdenv lib;

  pre-commit = (import pins."pre-commit-hooks.nix").run {
    src = ./.;
    hooks = {
      nixfmt-rfc-style = {
        enable = true;
        settings.width = 100;
      };
      rustfmt.enable = true;
    };
  };
in
pkgs.mkShell {
  nativeBuildInputs = with pkgs; [
    cargo
    clippy
    rustc
    rust-analyzer
    rustfmt
    nixfmt-rfc-style
    lix
    nix-prefetch-git
    git
    # I can't be assed to figure out the magic incantation that gets a rust package overriden,
    # but thankfully the npins repo ships its own derivation so just use that
    (pkgs.callPackage (pins.npins + "/npins.nix") { })
    just
  ];

  inherit (pre-commit) shellHook;
}
