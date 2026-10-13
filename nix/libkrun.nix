{
  lib,
  stdenv,
  rustPlatform,
  cargo,
  rustc,
  glibc,
  libcap_ng,
  src,
}:
let
  version = lib.head (
    builtins.match ".*\nFULL_VERSION=([^\n]+)\n.*" (builtins.readFile "${src}/Makefile")
  );
  variant = lib.optionalString stdenv.hostPlatform.isDarwin "-efi";
  features =
    if stdenv.hostPlatform.isDarwin then
      [ "efi" ]
    else
      [
        "blk"
        "net"
      ];
in
stdenv.mkDerivation {
  pname = "libkrun${variant}";
  inherit version src;

  cargoDeps = rustPlatform.importCargoLock { lockFile = "${src}/Cargo.lock"; };

  nativeBuildInputs = [
    rustPlatform.cargoSetupHook
    cargo
    rustc
  ];

  buildInputs = lib.optionals stdenv.hostPlatform.isLinux [
    libcap_ng
    glibc
    glibc.static
  ];

  # The init blob is cross-compiled for Linux, which on macOS needs a Debian
  # sysroot the Makefile downloads at build time. nixvm boots its own kernel and
  # initrd and never runs libkrun's init, so build it only where it's native.
  buildPhase = ''
    runHook preBuild
    cargo build --release --frozen -p libkrun \
      ${lib.optionalString stdenv.hostPlatform.isDarwin "--no-default-features"} \
      --features ${lib.concatStringsSep "," features}
  ''
  + (
    if stdenv.hostPlatform.isDarwin then
      ''
        install_name_tool -id $out/lib/libkrun${variant}.1.dylib target/release/libkrun.dylib
        cp target/release/libkrun.dylib target/release/libkrun${variant}.${version}.dylib
      ''
    else
      ''
        cp target/release/libkrun.so target/release/libkrun.so.${version}
      ''
  )
  + ''
    runHook postBuild
  '';

  installFlags = [
    "PREFIX=${placeholder "out"}"
    "LIBDIR_Linux=lib"
  ]
  ++ lib.optional stdenv.hostPlatform.isDarwin "EFI=1";

  # libkrun.pc says `-lkrun` even for the EFI flavour.
  postInstall = lib.optionalString stdenv.hostPlatform.isDarwin ''
    ln -s libkrun${variant}.dylib $out/lib/libkrun.dylib
  '';

  meta = {
    description = "Dynamic library providing Virtualization-based process isolation capabilities";
    homepage = "https://github.com/rvolosatovs/libkrun";
    license = lib.licenses.asl20;
    platforms = lib.platforms.darwin ++ lib.platforms.linux;
  };
}
