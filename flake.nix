# Dev environment for nixvm. `nix develop` then `make`.
#
# Intentionally only exposes a devShell — packaging nixvm itself is out of
# scope for the PoC. This flake only sets up the build environment.
{
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      system = "aarch64-darwin";
      pkgs = nixpkgs.legacyPackages.${system};
    in
    {
      devShells.${system}.default = pkgs.mkShell {
        nativeBuildInputs = with pkgs; [
          # Cargo build pipeline
          pkg-config
          rustc
          cargo
          rustfmt
          clippy
          rust-analyzer

          # bindgen needs libclang at runtime
          llvmPackages.libclang.lib

          # libkrun submodule build
          gnumake
          lld
          xz
        ];

        buildInputs = with pkgs; [
          # Nix C API libs (.pc files + headers via dev outputs)
          nix
        ];

        # bindgen finds libclang via this env var
        LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
      };
    };
}
