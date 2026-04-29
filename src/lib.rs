//! nixvm — launch a Nix flake output as an ephemeral, headless Linux VM
//! on macOS via libkrun.
//!
//! Flow: parse flake ref → eval+realise the image via the Nix C API →
//! copy to a per-launch overlay → set TTY raw → fork → child runs
//! `krun_start_enter` (which exits with the guest's exit code) → parent
//! waits, restores TTY, unlinks overlay.

use std::ffi::{CStr, CString};
use std::fs;
use std::os::raw::{c_char, c_int, c_uint, c_void};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::ptr;
use std::str;

use anyhow::{Context, Result, anyhow, bail};
use tracing::debug;

#[allow(non_camel_case_types, non_snake_case, non_upper_case_globals, dead_code)]
mod nix_sys {
    include!(concat!(env!("OUT_DIR"), "/nix_bindings.rs"));
}

const NIXVM_EDK2_PATH: &str = env!("NIXVM_EDK2_PATH");

#[derive(Debug, Clone)]
pub struct Args {
    pub flake_ref: String,
    pub cpus: u8,
    pub memory_mib: u32,
}

pub fn run(args: Args) -> Result<u8> {
    let (flake_uri, attr_path) = split_flake_ref(&args.flake_ref)?;

    let flake_uri = canonicalize_flake_uri(&flake_uri)?;
    debug!(flake = %flake_uri, attr = %attr_path, "evaluating + realising flake output");
    let image_path = nix_realise_image(&flake_uri, &attr_path)
        .context("failed to evaluate or realise the flake output")?;
    debug!(image = %image_path.display(), "built image");

    // Copy the immutable built image to an ephemeral per-launch overlay.
    let overlay = Overlay::from_base(&image_path).context("failed to prepare overlay")?;

    // Put the host TTY in raw mode BEFORE fork. libkrun also calls
    // setup_terminal_raw_mode internally, but only after start_enter has
    // configured the guest console — between fork and that point, the host
    // kernel's line discipline can still chew newlines / buffer input,
    // which the user sees as keystrokes accumulating across commands.
    // Saving + restoring with a Drop guard cleans up on any exit path.
    let _tty = RawTerminal::enter();

    let exit_code = fork_and_run_vm(&overlay, args.cpus, args.memory_mib)
        .context("failed to launch the VM")?;
    Ok(exit_code)
}

// ────────────────────────── flake ref parsing ──────────────────────────

/// Resolve relative paths to absolute so `builtins.getFlake` doesn't fail
/// on path-style refs. Pass through everything else unchanged.
fn canonicalize_flake_uri(uri: &str) -> Result<String> {
    let stripped = uri.strip_prefix("path:").unwrap_or(uri);
    let looks_like_path = stripped.starts_with("./")
        || stripped.starts_with("../")
        || stripped == "."
        || stripped == ".."
        || stripped.starts_with('/');
    if !looks_like_path {
        return Ok(uri.to_string());
    }
    let abs = fs::canonicalize(stripped)
        .with_context(|| format!("could not canonicalize path-style flake ref `{uri}`"))?;
    Ok(format!("path:{}", abs.display()))
}

fn split_flake_ref(s: &str) -> Result<(String, String)> {
    // Split on the LAST `#` to allow URIs that contain `#` in lockless params
    // (rare but real). `git+ssh://...?ref=foo#attr` still parses correctly.
    match s.rsplit_once('#') {
        Some((uri, attr)) if !uri.is_empty() && !attr.is_empty() => {
            Ok((uri.to_string(), attr.to_string()))
        }
        _ => bail!("flake ref must be of the form `<flake>#<attr-path>`, got `{s}`"),
    }
}

// ─────────────────────────── Nix C API glue ───────────────────────────

/// Owning wrapper for `nix_c_context*`. After every call, check `nix_err_code`
/// and surface the message via [`Self::check`].
struct NixCtx {
    raw: *mut nix_sys::nix_c_context,
}

impl NixCtx {
    fn new() -> Result<Self> {
        let raw = unsafe { nix_sys::nix_c_context_create() };
        if raw.is_null() {
            bail!("nix_c_context_create returned NULL");
        }
        Ok(Self { raw })
    }

    fn check(&self) -> Result<()> {
        let code = unsafe { nix_sys::nix_err_code(self.raw) };
        if code == nix_sys::nix_err_NIX_OK {
            return Ok(());
        }
        let mut n: c_uint = 0;
        let msg_ptr = unsafe { nix_sys::nix_err_msg(ptr::null_mut(), self.raw, &mut n) };
        let msg = if msg_ptr.is_null() {
            String::from("(no message)")
        } else {
            unsafe { CStr::from_ptr(msg_ptr).to_string_lossy().into_owned() }
        };
        Err(anyhow!("nix error {code}: {msg}"))
    }
}

impl Drop for NixCtx {
    fn drop(&mut self) {
        unsafe { nix_sys::nix_c_context_free(self.raw) }
    }
}

/// Callback adapter for `nix_get_string` style APIs that stream chunks.
unsafe extern "C" fn collect_string_cb(start: *const c_char, n: c_uint, user_data: *mut c_void) {
    if start.is_null() || user_data.is_null() {
        return;
    }
    let buf = unsafe { &mut *(user_data as *mut Vec<u8>) };
    let slice = unsafe { std::slice::from_raw_parts(start as *const u8, n as usize) };
    buf.extend_from_slice(slice);
}

fn read_string(
    ctx: &NixCtx,
    f: impl FnOnce(*mut nix_sys::nix_c_context, nix_sys::nix_get_string_callback, *mut c_void) -> i32,
) -> Result<String> {
    let mut buf: Vec<u8> = Vec::new();
    let rc = f(
        ctx.raw,
        Some(collect_string_cb),
        &mut buf as *mut _ as *mut c_void,
    );
    if rc != nix_sys::nix_err_NIX_OK {
        ctx.check()?;
    }
    String::from_utf8(buf).context("nix returned non-UTF8 string")
}

/// Drives the full eval+realise pipeline. Returns the realised store path
/// of the resulting raw EFI disk image.
fn nix_realise_image(flake_uri: &str, attr_path: &str) -> Result<PathBuf> {
    let ctx = NixCtx::new()?;

    unsafe {
        nix_sys::nix_libutil_init(ctx.raw);
        ctx.check().context("nix_libutil_init")?;

        // Enable flakes (for `builtins.getFlake`) before libstore/libexpr
        // pick up settings.
        let key = CString::new("experimental-features").unwrap();
        let val = CString::new("nix-command flakes").unwrap();
        nix_sys::nix_setting_set(ctx.raw, key.as_ptr(), val.as_ptr());
        ctx.check().context("enable experimental flakes feature")?;

        nix_sys::nix_libstore_init(ctx.raw);
        ctx.check().context("nix_libstore_init")?;
        nix_sys::nix_libexpr_init(ctx.raw);
        ctx.check().context("nix_libexpr_init")?;
    }

    // Open the local store via the daemon protocol (handles privileged builds,
    // cache sharing, etc. — same as `nix build`).
    let uri = CString::new("daemon").unwrap();
    let store = unsafe { nix_sys::nix_store_open(ctx.raw, uri.as_ptr(), ptr::null_mut()) };
    ctx.check().context("nix_store_open(daemon)")?;
    if store.is_null() {
        bail!("nix_store_open returned NULL");
    }
    let _store_guard = scopeguard(|| unsafe { nix_sys::nix_store_free(store) });

    // Build the eval state with flake support so `builtins.getFlake` exists.
    let flake_settings = unsafe { nix_sys::nix_flake_settings_new(ctx.raw) };
    ctx.check().context("nix_flake_settings_new")?;
    if flake_settings.is_null() {
        bail!("nix_flake_settings_new returned NULL");
    }
    let _flake_settings_guard =
        scopeguard(|| unsafe { nix_sys::nix_flake_settings_free(flake_settings) });

    let builder = unsafe { nix_sys::nix_eval_state_builder_new(ctx.raw, store) };
    ctx.check().context("nix_eval_state_builder_new")?;
    if builder.is_null() {
        bail!("nix_eval_state_builder_new returned NULL");
    }
    let _builder_guard = scopeguard(|| unsafe { nix_sys::nix_eval_state_builder_free(builder) });

    unsafe { nix_sys::nix_eval_state_builder_load(ctx.raw, builder) };
    ctx.check().context("nix_eval_state_builder_load")?;

    unsafe {
        nix_sys::nix_flake_settings_add_to_eval_state_builder(ctx.raw, flake_settings, builder);
    }
    ctx.check()
        .context("nix_flake_settings_add_to_eval_state_builder")?;

    let state = unsafe { nix_sys::nix_eval_state_build(ctx.raw, builder) };
    ctx.check().context("nix_eval_state_build")?;
    if state.is_null() {
        bail!("nix_eval_state_build returned NULL");
    }
    let _state_guard = scopeguard(|| unsafe { nix_sys::nix_state_free(state) });

    // Evaluate the derivation, pulling out both drvPath (to realise) and
    // outPath (the path the realised output will live at). Avoids needing to
    // walk the realise callback to recover the absolute output path.
    let expr = CString::new(format!(
        r#"let drv = (builtins.getFlake "{uri}").{attr};
            in {{ drvPath = drv.drvPath; outPath = drv.outPath; }}"#,
        uri = flake_uri.escape_default(),
        attr = attr_path,
    ))
    .unwrap();
    let cwd = CString::new(".").unwrap();

    let value = unsafe { nix_sys::nix_alloc_value(ctx.raw, state) };
    ctx.check().context("nix_alloc_value")?;
    let _value_guard = scopeguard(|| unsafe {
        let _ = nix_sys::nix_value_decref(ctx.raw, value);
    });

    unsafe {
        nix_sys::nix_expr_eval_from_string(ctx.raw, state, expr.as_ptr(), cwd.as_ptr(), value);
    }
    ctx.check().context("nix_expr_eval_from_string")?;
    unsafe {
        nix_sys::nix_value_force(ctx.raw, state, value);
    }
    ctx.check().context("nix_value_force")?;

    let drv_path = read_attr_string(&ctx, state, value, "drvPath")?;
    let out_path = read_attr_string(&ctx, state, value, "outPath")?;

    // Parse the drv path and realise it.
    let drv_cstr = CString::new(drv_path.clone()).unwrap();
    let store_path =
        unsafe { nix_sys::nix_store_parse_path(ctx.raw, store, drv_cstr.as_ptr()) };
    ctx.check().context("nix_store_parse_path")?;
    if store_path.is_null() {
        bail!("nix_store_parse_path returned NULL for {drv_path}");
    }
    let _path_guard = scopeguard(|| unsafe { nix_sys::nix_store_path_free(store_path) });

    // Realise (build) the derivation. We don't need the callback's output
    // paths since we already evaluated `outPath`.
    unsafe {
        nix_sys::nix_store_realise(
            ctx.raw,
            store,
            store_path,
            ptr::null_mut(),
            None,
        );
    }
    ctx.check().context("nix_store_realise")?;

    Ok(PathBuf::from(out_path))
}

fn read_attr_string(
    ctx: &NixCtx,
    state: *mut nix_sys::EvalState,
    parent: *mut nix_sys::nix_value,
    name: &str,
) -> Result<String> {
    let cname = CString::new(name).unwrap();
    let attr = unsafe { nix_sys::nix_get_attr_byname(ctx.raw, parent, state, cname.as_ptr()) };
    ctx.check().with_context(|| format!("nix_get_attr_byname({name})"))?;
    if attr.is_null() {
        bail!("attribute `{name}` missing");
    }
    let _attr_guard = scopeguard(|| unsafe {
        let _ = nix_sys::nix_value_decref(ctx.raw, attr);
    });
    unsafe { nix_sys::nix_value_force(ctx.raw, state, attr) };
    ctx.check().with_context(|| format!("force {name}"))?;
    read_string(ctx, |ctx_raw, cb, ud| unsafe {
        nix_sys::nix_get_string(ctx_raw, attr, cb, ud)
    })
    .with_context(|| format!("read {name}"))
}

// Minimal scope-guard helper (avoids pulling in a crate).
fn scopeguard<F: FnOnce()>(f: F) -> ScopeGuard<F> {
    ScopeGuard { f: Some(f) }
}
struct ScopeGuard<F: FnOnce()> {
    f: Option<F>,
}
impl<F: FnOnce()> Drop for ScopeGuard<F> {
    fn drop(&mut self) {
        if let Some(f) = self.f.take() {
            f();
        }
    }
}

// ──────────────────────────── overlay file ────────────────────────────

struct Overlay {
    path: PathBuf,
}

impl Overlay {
    fn from_base(base: &Path) -> Result<Self> {
        // tempfile::Builder gives us a uniquely-named file we own; we delete
        // it on Drop.
        let tmp = tempfile::Builder::new()
            .prefix("nixvm-overlay-")
            .suffix(".img")
            .tempfile()
            .context("create tempfile for overlay")?;
        let path = tmp.path().to_owned();
        // Close the FD so std::fs::copy can write freely on macOS.
        drop(tmp.into_temp_path().keep().context("retain overlay path")?);
        fs::copy(base, &path).with_context(|| {
            format!("copy {} → {}", base.display(), path.display())
        })?;
        // fs::copy preserves the source's permissions; the source is in
        // /nix/store and read-only. libkrun opens disk images read-write,
        // which fails on a 0444 file.
        let mut perms = fs::metadata(&path)?.permissions();
        perms.set_mode(0o600);
        fs::set_permissions(&path, perms)?;
        Ok(Self { path })
    }
}

impl Drop for Overlay {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

// ──────────────────────────── raw terminal ────────────────────────────

/// Best-effort raw mode on the host TTY (fd 0). libkrun would also do this
/// later, but doing it eagerly avoids the kernel line discipline buffering
/// keystrokes during the window between fork and start_enter.
struct RawTerminal {
    saved: Option<rustix::termios::Termios>,
}

impl RawTerminal {
    fn enter() -> Self {
        let stdin = std::io::stdin();
        if !rustix::termios::isatty(&stdin) {
            return Self { saved: None };
        }
        let saved = match rustix::termios::tcgetattr(&stdin) {
            Ok(t) => t,
            Err(_) => return Self { saved: None },
        };
        let mut raw = saved.clone();
        raw.make_raw();
        let _ = rustix::termios::tcsetattr(
            &stdin,
            rustix::termios::OptionalActions::Now,
            &raw,
        );
        Self { saved: Some(saved) }
    }
}

impl Drop for RawTerminal {
    fn drop(&mut self) {
        if let Some(saved) = &self.saved {
            let stdin = std::io::stdin();
            let _ = rustix::termios::tcsetattr(
                &stdin,
                rustix::termios::OptionalActions::Now,
                saved,
            );
        }
    }
}

// ─────────────────────────── libkrun + fork ───────────────────────────

fn fork_and_run_vm(overlay: &Overlay, cpus: u8, mem_mib: u32) -> Result<u8> {
    // We deliberately do NOT call krun_create_ctx in the parent: libkrun's
    // krun_start_enter() never returns and exit()s the process. Doing all
    // libkrun calls in the child means the parent retains its identity for
    // cleanup (overlay unlink, TTY restore) and for surfacing exit status.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        bail!("fork: {}", std::io::Error::last_os_error());
    }

    if pid == 0 {
        // Child: configure libkrun and start the VM. krun_start_enter() does
        // not return; on failure we _exit() with a distinguishable code so
        // the parent can surface it.
        if let Err(err) = configure_and_start_vm(overlay.path.as_path(), cpus, mem_mib) {
            // Use raw stderr write here, not tracing — the subscriber may
            // already be tearing down post-fork. The user sees this if the
            // VM never started.
            eprintln!("nixvm: child VM setup failed: {err:#}");
        }
        // If we got here, something failed before krun_start_enter took over.
        unsafe { libc::_exit(127) };
    }

    // Parent: drop our claim on stdin so the child (libkrun) is the sole
    // reader. Otherwise both processes share fd 0 and the kernel can hand
    // the same character to whichever happens to read first, which shows
    // up as input being eaten or duplicated.
    unsafe {
        let devnull = libc::open(c"/dev/null".as_ptr(), libc::O_RDONLY);
        if devnull >= 0 {
            libc::dup2(devnull, 0);
            libc::close(devnull);
        }
    }

    // Parent: wait for child.
    let mut status: c_int = 0;
    let waited = unsafe { libc::waitpid(pid, &mut status, 0) };
    if waited < 0 {
        bail!("waitpid: {}", std::io::Error::last_os_error());
    }
    if libc::WIFEXITED(status) {
        Ok(libc::WEXITSTATUS(status) as u8)
    } else if libc::WIFSIGNALED(status) {
        let sig = libc::WTERMSIG(status);
        eprintln!("nixvm: VM terminated by signal {sig}");
        Ok(128 + sig as u8)
    } else {
        bail!("VM exited with unknown status {status}")
    }
}

fn configure_and_start_vm(overlay_path: &Path, cpus: u8, mem_mib: u32) -> Result<()> {
    use krun_sys::*;

    // Silence libkrun's own logger — its `error!` lines (e.g. the vsock
    // muxer's "unexpected dgram pkt") leak onto the host TTY and interleave
    // with the guest shell. Set NIXVM_LOG to see nixvm's own tracing.
    unsafe { krun_set_log_level(0) };

    let ctx = unsafe { krun_create_ctx() };
    if ctx < 0 {
        bail!("krun_create_ctx: {ctx}");
    }
    let ctx = ctx as u32;

    krun_check(unsafe { krun_set_vm_config(ctx, cpus, mem_mib) }, "krun_set_vm_config")?;

    let firmware = CString::new(NIXVM_EDK2_PATH).unwrap();
    krun_check(
        unsafe { krun_set_firmware(ctx, firmware.as_ptr()) },
        "krun_set_firmware",
    )?;

    // Root disk: the ephemeral overlay (raw image). Using the simpler
    // `krun_set_root_disk` API (same one libkrun's boot_efi.c example uses);
    // krun_add_disk2 surfaced "Error configuring virtio-blk" at start time.
    let disk_path = CString::new(overlay_path.to_str().context("overlay path not UTF-8")?).unwrap();
    krun_check(
        unsafe { krun_set_root_disk(ctx, disk_path.as_ptr()) },
        "krun_set_root_disk",
    )?;

    // Share the host /nix/store into the guest, read-only.
    let tag = CString::new("nixstore").unwrap();
    let host_store = CString::new("/nix/store").unwrap();
    krun_check(
        unsafe {
            krun_add_virtiofs3(
                ctx,
                tag.as_ptr(),
                host_store.as_ptr(),
                /* shm_size: 0 → libkrun default */ 0,
                /* read_only */ true,
            )
        },
        "krun_add_virtiofs3(nixstore)",
    )?;

    // No krun_add_virtio_console_default: libkrun's *implicit* console
    // already wires guest /dev/hvc0 to the host's fd 0/1/2 when those are
    // a TTY (see autoconfigure_console_ports in libkrun). Adding an
    // explicit console on top creates a second virtio-console port, which
    // appeared to corrupt input. Same pattern libkrun's chroot_vm.c uses.
    debug!("krun_start_enter");

    // No network device added → libkrun automatically enables TSI
    // (transparent socket impersonation) for outbound TCP/UDP.
    //
    // We don't use a vsock device, but libkrun creates one implicitly.
    // Disable it to stop the vsock muxer logging unexpected packets.
    debug!("krun_disable_implicit_vsock");
    krun_check(
        unsafe { krun_disable_implicit_vsock(ctx) },
        "krun_disable_implicit_vsock",
    )?;

    // Hand control to libkrun. Does not return on success.
    krun_check(unsafe { krun_start_enter(ctx) }, "krun_start_enter")?;
    Ok(())
}

fn krun_check(rc: i32, what: &str) -> Result<()> {
    if rc == 0 {
        Ok(())
    } else {
        Err(anyhow!("{what} failed: {rc} ({})", std::io::Error::from_raw_os_error(-rc)))
    }
}
