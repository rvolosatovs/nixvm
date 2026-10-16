# nixvm package, dev environment and the shared NixOS guest module:
#   - packages.aarch64-darwin.default  — `nix shell github:rvolosatovs/nixvm`
#   - devShells.aarch64-darwin.default — `nix develop` then `make`
#   - nixosModules.guest               — host↔guest contract for image flakes
#
# `github:` flake refs don't carry submodule contents, so the package takes
# libkrun from the `libkrun` input instead of `vendor/libkrun`. CI checks that
# the two stay on the same revision.
{
  inputs.nixify.url = "github:rvolosatovs/nixify";
  inputs.libkrun.url = "github:rvolosatovs/libkrun/nixvm-v1.19";
  inputs.libkrun.flake = false;

  outputs =
    {
      self,
      nixify,
      libkrun,
      ...
    }:
    with nixify.lib;
    let
      mkPackages = pkgs: rec {
        libkrun-efi = pkgs.callPackage ./nix/libkrun.nix { src = libkrun; };
        nixvm = pkgs.callPackage ./nix/nixvm.nix {
          src = pkgs.lib.fileset.toSource {
            root = ./.;
            fileset = pkgs.lib.fileset.unions [
              ./Cargo.toml
              ./Cargo.lock
              ./build.rs
              ./src
              ./c_src
              ./entitlements.plist
            ];
          };
          libkrun = libkrun-efi;
        };
      };
      overlay = final: _: { inherit (mkPackages final) nixvm; };
    in
    rust.mkFlake {
      src = self;

      # nixify's crane-based packages and checks can't build the vendored
      # libkrun or codesign the binary; nix/*.nix do both.
      withPackages =
        { pkgs, ... }:
        pkgs.lib.optionalAttrs (pkgs.stdenv.hostPlatform.system == "aarch64-darwin") (
          let
            packages = mkPackages pkgs;
          in
          packages // { default = packages.nixvm; }
        );
      withOverlays =
        { overlays, ... }:
        overlays
        // {
          default = overlay;
          nixvm = overlay;
        };
      withChecks = _: { };

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

          # build.rs compiles the C++ shim via the `cc` crate, which passes an
          # Apple LLVM triple (`arm64-apple-macosx<ver>`) as `--target`. Nixpkgs'
          # cc-wrapper warns on any `--target` != its baked-in `arm64-apple-darwin`
          # ("multi-target compilers"). The codegen is identical here, so silence
          # the warning via the wrapper's own documented knob rather than swapping
          # in an unwrapped compiler.
          env.NIX_CC_WRAPPER_SUPPRESS_TARGET_WARNING = "1";

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
