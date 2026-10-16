{
  lib,
  rustPlatform,
  pkg-config,
  makeWrapper,
  darwin,
  xcbuild,
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

  postUnpack = ''
    mkdir -p $sourceRoot/vendor/libkrun
    cp -r ${libkrun.src}/krun-sys $sourceRoot/vendor/libkrun/krun-sys
    chmod -R u+w $sourceRoot/vendor/libkrun
  '';

  cargoLock.lockFile = "${src}/Cargo.lock";

  nativeBuildInputs = [
    pkg-config
    rustPlatform.bindgenHook
    makeWrapper
    darwin.sigtool
    xcbuild
  ];

  buildInputs = [
    libkrun
    nix
  ];

  env.NIX_CC_WRAPPER_SUPPRESS_TARGET_WARNING = "1";

  doCheck = false;

  postInstall = ''
    wrapProgram $out/bin/nixvm --prefix PATH : ${lib.makeBinPath [ nix ]}
  '';

  postFixup = ''
    codesign --force --sign - --entitlements $src/entitlements.plist $out/bin/.nixvm-wrapped
  '';

  meta = {
    description = "Launch a NixOS flake output as an ephemeral VM via libkrun";
    homepage = "https://github.com/rvolosatovs/nixvm";
    platforms = [ "aarch64-darwin" ];
    mainProgram = "nixvm";
  };
}
