use std::env;
use std::path::PathBuf;

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());

    // Path to the EDK2 UEFI firmware blob shipped inside libkrun.
    let edk2_firmware = manifest_dir.join("vendor/libkrun/edk2/KRUN_EFI.silent.fd");
    assert!(
        edk2_firmware.exists(),
        "EDK2 firmware not found at {} — did you `git submodule update --init`?",
        edk2_firmware.display()
    );
    println!(
        "cargo:rustc-env=NIXVM_EDK2_PATH={}",
        edk2_firmware.display()
    );

    // Nix C API: pkg-config to locate, bindgen to generate bindings.
    let probe = |name: &str| {
        pkg_config::Config::new()
            .atleast_version("2.30")
            .probe(name)
            .unwrap_or_else(|e| panic!("pkg-config could not find {name}: {e}"))
    };
    let _ = probe("nix-util-c");
    let _ = probe("nix-store-c");
    let _ = probe("nix-expr-c");
    let nix_flake = probe("nix-flake-c");

    // nix-flake-c's --cflags transitively includes the rest.
    let clang_args: Vec<String> = nix_flake
        .include_paths
        .iter()
        .map(|p| format!("-I{}", p.display()))
        .collect();

    let bindings = bindgen::Builder::default()
        .header_contents(
            "nix_wrapper.h",
            r#"
                #include <nix_api_util.h>
                #include <nix_api_store.h>
                #include <nix_api_expr.h>
                #include <nix_api_value.h>
                #include <nix_api_fetchers.h>
                #include <nix_api_flake.h>
            "#,
        )
        .clang_args(&clang_args)
        .allowlist_function("nix_.*")
        .allowlist_type("nix_.*|Nix.*|Eval.*|Store.*")
        .allowlist_var("NIX_.*")
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        .generate()
        .expect("bindgen failed to generate Nix C API bindings");

    bindings
        .write_to_file(out_dir.join("nix_bindings.rs"))
        .expect("failed to write nix_bindings.rs");

    println!("cargo:rerun-if-changed=build.rs");
}
