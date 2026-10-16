{
  lib,
  stdenv,
  rustPlatform,
  pkg-config,
  darwin,
  nix,
  libkrun,
  src,
}:
let
  cargoToml = lib.importTOML "${src}/Cargo.toml";
in
rustPlatform.buildRustPackage {
  pname = cargoToml.package.name;
  inherit (cargoToml.package) version;
  inherit src;

  # `github:` flake refs are tarballs without submodule contents.
  postUnpack = ''
    rm -rf $sourceRoot/vendor/libkrun
    mkdir -p $sourceRoot/vendor
    cp -r ${libkrun.src} $sourceRoot/vendor/libkrun
    chmod -R u+w $sourceRoot/vendor/libkrun
  '';

  cargoLock.lockFile = "${src}/Cargo.lock";

  nativeBuildInputs = [
    pkg-config
    rustPlatform.bindgenHook
  ]
  ++ lib.optional stdenv.hostPlatform.isDarwin darwin.sigtool;

  buildInputs = [
    libkrun
    nix
  ];

  env.NIX_CC_WRAPPER_SUPPRESS_TARGET_WARNING = "1";

  doCheck = false;

  postFixup = lib.optionalString stdenv.hostPlatform.isDarwin ''
    codesign --force --sign - --entitlements $src/entitlements.plist $out/bin/nixvm
  '';

  meta = {
    description = "Launch a NixOS flake output as an ephemeral VM via libkrun";
    homepage = "https://github.com/rvolosatovs/nixvm";
    platforms = [ "aarch64-darwin" ];
    mainProgram = "nixvm";
  };
}
