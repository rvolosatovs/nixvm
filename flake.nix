# Dev environment for nixvm + the shared NixOS guest module.
#
# Packaging nixvm itself is out of scope for the PoC (libkrun submodule build +
# codesigning don't fit nixify's Rust flow), so this flake exposes:
#   - devShells.aarch64-darwin.default — `nix develop` then `make`
#   - nixosModules.guest               — host↔guest contract for image flakes
{
  inputs.nixify.url = "github:rvolosatovs/nixify";

  outputs =
    { self, nixify, ... }:
    with nixify.lib;
    rust.mkFlake {
      src = self;

      # Packaging is out of scope — drop the auto-generated rust packages,
      # checks and apps so the flake only exposes the devShell.
      withPackages = _: { };
      withChecks = _: { };
      withApps = _: { };

      withDevShells =
        { pkgs, devShells, ... }:
        extendDerivations {
          nativeBuildInputs = with pkgs; [
            gnumake
            lld
            llvmPackages.libclang.lib
            pkg-config
            xz
          ];

          buildInputs = with pkgs; [
            nix
          ];

          env.LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";

          # Point cargo at the vendored libkrun built by `make libkrun` so
          # direct `cargo` invocations (clippy, check, build, rust-analyzer)
          # work without going through the Makefile. PWD is the flake root
          # when entering `nix develop` from this directory; `make libkrun`
          # populates ./build/prefix.
          shellHook = ''
            export PKG_CONFIG_PATH="$PWD/build/prefix/lib/pkgconfig''${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"
            export LIBRARY_PATH="$PWD/build/prefix/lib''${LIBRARY_PATH:+:$LIBRARY_PATH}"
            export DYLD_FALLBACK_LIBRARY_PATH="$PWD/build/prefix/lib''${DYLD_FALLBACK_LIBRARY_PATH:+:$DYLD_FALLBACK_LIBRARY_PATH}"
          '';
        } devShells;
    }
    // {
      nixosModules.guest = ./modules/guest.nix;
      nixosModules.default = ./modules/guest.nix;
    };
}
