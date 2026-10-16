{
  lib,
  stdenv,
  rustPlatform,
  cargo,
  rustc,
  src,
}:
stdenv.mkDerivation {
  pname = "libkrun-efi";
  inherit ((lib.importTOML "${src}/src/libkrun/Cargo.toml").package) version;
  inherit src;

  cargoDeps = rustPlatform.importCargoLock { lockFile = "${src}/Cargo.lock"; };

  nativeBuildInputs = [
    rustPlatform.cargoSetupHook
    cargo
    rustc
  ];

  makeFlags = [
    "PREFIX=${placeholder "out"}"
    "EFI=1"
    "SYSROOT_LINUX=/var/empty"
  ];

  preBuild = ''
    makeFlagsArray+=(FEATURE_FLAGS="--frozen -p libkrun --no-default-features --features efi")
  '';

  postInstall = ''
    ln -s libkrun-efi.dylib $out/lib/libkrun.dylib
  '';

  meta = {
    description = "Dynamic library providing Virtualization-based process isolation capabilities";
    homepage = "https://github.com/rvolosatovs/libkrun";
    license = lib.licenses.asl20;
    platforms = [ "aarch64-darwin" ];
  };
}
