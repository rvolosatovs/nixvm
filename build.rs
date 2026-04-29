use std::env;
use std::path::PathBuf;

fn main() {
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());

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

    // ── macOS system frameworks: vmnet + dispatch + xpc + Block ──────────
    //
    // We talk to vmnet.framework directly (in-process packet pump). Block
    // ABI lives in /usr/include via the SDK; xpc and dispatch live in
    // libSystem (no extra link flag needed).
    let sdk = std::process::Command::new("xcrun")
        .args(["--show-sdk-path"])
        .output()
        .expect("xcrun not on PATH (install Xcode CLT)");
    let sdk_path = String::from_utf8(sdk.stdout)
        .expect("xcrun returned non-UTF8 path")
        .trim()
        .to_owned();

    let sys_bindings = bindgen::Builder::default()
        .header_contents(
            "sys_wrapper.h",
            r#"
                #include <vmnet/vmnet.h>
                #include <dispatch/dispatch.h>
                #include <xpc/xpc.h>
                #include <Block.h>
            "#,
        )
        .clang_args([
            "-isysroot",
            &sdk_path,
            // Apple's headers use blocks — bindgen needs the extension on.
            "-fblocks",
        ])
        .allowlist_function("vmnet_.*|xpc_.*|dispatch_.*|_Block_.*")
        .allowlist_type("vmnet_.*|interface_.*|operating_modes_t|vmpktdesc|dispatch_.*|xpc_.*")
        .allowlist_var("vmnet_.*|VMNET_.*|XPC_.*|DISPATCH_.*")
        // Apple Block typedefs (`void (^)(…)`) — bindgen renders them
        // as `u64` by default. Tell it to treat block typedefs as
        // opaque c_void pointers; we hand it `&*RcBlock as *mut c_void`
        // (cast to the right param type at the call site).
        .raw_line("pub type _NixvmBlock = *mut ::std::os::raw::c_void;")
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        .generate()
        .expect("bindgen failed for vmnet/xpc/dispatch headers");

    sys_bindings
        .write_to_file(out_dir.join("sys_bindings.rs"))
        .expect("failed to write sys_bindings.rs");

    println!("cargo:rustc-link-arg=-framework");
    println!("cargo:rustc-link-arg=vmnet");

    println!("cargo:rerun-if-changed=build.rs");
}
