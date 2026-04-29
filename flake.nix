# Dev environment for nixvm + the shared NixOS guest module.
#
# Packaging nixvm itself is out of scope for the PoC, so this flake exposes:
#   - devShells.aarch64-darwin.default — `nix develop` then `make`
#   - nixosModules.guest               — host↔guest contract for image flakes
{
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      system = "aarch64-darwin";
      pkgs = nixpkgs.legacyPackages.${system};
    in
    {
      nixosModules.guest = ./modules/guest.nix;
      nixosModules.default = self.nixosModules.guest;

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
