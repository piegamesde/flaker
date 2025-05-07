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
  nativeBuildInputs =
    with pkgs;
    [
      cargo
      clippy
      rustc
      rust-analyzer
      rustfmt
      nixfmt-rfc-style
      lix
      nix-prefetch-git
      git
      npins
      just
    ]
    ++ (lib.optionals stdenv.isDarwin [
      pkgs.libiconv
      pkgs.darwin.apple_sdk.frameworks.Security
    ]);

  inherit (pre-commit) shellHook;
}
