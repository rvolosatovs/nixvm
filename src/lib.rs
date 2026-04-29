//! nixvm — launch a Nix flake output as an ephemeral, headless Linux VM
//! on macOS via libkrun.
//!
//! Flow: parse flake ref → eval+realise the image via the Nix C API →
//! copy to a per-launch overlay → set TTY raw → fork → child runs
//! `krun_start_enter` (which exits with the guest's exit code) → parent
//! waits, restores TTY, unlinks overlay.

use std::ffi::{CStr, CString};
use std::fs;
use std::os::fd::{AsFd, AsRawFd, OwnedFd, RawFd};
use std::os::raw::{c_char, c_int, c_uint, c_void};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::ptr;
use std::str;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

use anyhow::{Context, Result, anyhow, bail};
use tracing::{debug, info, warn};

#[allow(
    non_camel_case_types,
    non_snake_case,
    non_upper_case_globals,
    dead_code
)]
mod nix_sys {
    include!(concat!(env!("OUT_DIR"), "/nix_bindings.rs"));
}

#[allow(
    non_camel_case_types,
    non_snake_case,
    non_upper_case_globals,
    dead_code
)]
mod sys {
    include!(concat!(env!("OUT_DIR"), "/sys_bindings.rs"));
}

const NIXVM_EDK2_PATH: &str = env!("NIXVM_EDK2_PATH");

#[derive(Debug, Clone)]
pub struct RunArgs {
    pub flake_ref: String,
    /// If `Some`, copy the image to this path and keep it across exit.
    /// Resume later with `nixvm load <path>`.
    pub persist: Option<PathBuf>,
    pub cpus: u8,
    pub memory_mib: u32,
}

#[derive(Debug, Clone)]
pub struct LoadArgs {
    /// Existing image to boot in place. Writes are persisted back to it.
    pub path: PathBuf,
    pub cpus: u8,
    pub memory_mib: u32,
}

/// Build a flake's image and boot it. Ephemeral overlay unless `persist` is set.
pub fn run(args: RunArgs) -> Result<u8> {
    let id = uuid::Uuid::now_v7();
    info!(%id, "starting");

    let (flake_uri, attr_path) = split_flake_ref(&args.flake_ref)?;
    let flake_uri = canonicalize_flake_uri(&flake_uri)?;
    debug!(flake = %flake_uri, attr = %attr_path, "evaluating + realising flake output");
    let image_path = nix_realise_image(&flake_uri, &attr_path)
        .context("failed to evaluate or realise the flake output")?;
    debug!(image = %image_path.display(), "built image");

    let overlay = match args.persist {
        Some(path) => Overlay::persistent(&image_path, path),
        None => Overlay::ephemeral(&image_path, id),
    }
    .context("failed to prepare overlay")?;

    launch_vm(overlay, id, args.cpus, args.memory_mib)
}

/// Boot a previously-saved image (from `nixvm run -p`) in place. Writes
/// during the run mutate the file; resume by running `load` again.
pub fn load(args: LoadArgs) -> Result<u8> {
    let id = uuid::Uuid::now_v7();
    info!(%id, "starting");

    let overlay = Overlay::load(args.path).context("failed to open image")?;
    launch_vm(overlay, id, args.cpus, args.memory_mib)
}

/// Shared launch path: vmnet, raw TTY, fork, libkrun, wait, cleanup.
fn launch_vm(overlay: Overlay, id: uuid::Uuid, cpus: u8, mem_mib: u32) -> Result<u8> {
    let _span = tracing::info_span!("vm", %id).entered();

    // Open vmnet *before* fork. The interface_ref + dispatch queue are
    // valid only in the process that created them, so the parent owns
    // them and pumps packets; the child inherits the bare FD via fork.
    let vmnet = Vmnet::start().context("failed to start vmnet")?;

    // Put the host TTY in raw mode BEFORE fork. libkrun also calls
    // setup_terminal_raw_mode internally, but only after start_enter has
    // configured the guest console — between fork and that point, the host
    // kernel's line discipline can still chew newlines / buffer input,
    // which the user sees as keystrokes accumulating across commands.
    // Saving + restoring with a Drop guard cleans up on any exit path.
    let _tty = RawTerminal::enter();

    let exit_code =
        fork_and_run_vm(&overlay, &vmnet, cpus, mem_mib).context("failed to launch the VM")?;
    // vmnet drops here, AFTER waitpid → pump thread joins, vmnet_stop_interface fires.
    drop(vmnet);
    drop(overlay);
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
    let store_path = unsafe { nix_sys::nix_store_parse_path(ctx.raw, store, drv_cstr.as_ptr()) };
    ctx.check().context("nix_store_parse_path")?;
    if store_path.is_null() {
        bail!("nix_store_parse_path returned NULL for {drv_path}");
    }
    let _path_guard = scopeguard(|| unsafe { nix_sys::nix_store_path_free(store_path) });

    // Realise (build) the derivation. We don't need the callback's output
    // paths since we already evaluated `outPath`.
    unsafe {
        nix_sys::nix_store_realise(ctx.raw, store, store_path, ptr::null_mut(), None);
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
    ctx.check()
        .with_context(|| format!("nix_get_attr_byname({name})"))?;
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
    mode: OverlayMode,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum OverlayMode {
    /// Per-launch tempfile in `$TMPDIR`, deleted on Drop.
    Ephemeral,
    /// Created at user-specified path by `run -p`, retained on Drop.
    Persistent,
    /// Existing file opened by `load`, retained on Drop.
    Loaded,
}

impl Overlay {
    /// `nixvm run` (default): copy base to `$TMPDIR/nixvm-<uuid>.img`.
    fn ephemeral(base: &Path, id: uuid::Uuid) -> Result<Self> {
        let path = std::env::temp_dir().join(format!("nixvm-{id}.img"));
        copy_writable(base, &path)?;
        Ok(Self {
            path,
            mode: OverlayMode::Ephemeral,
        })
    }

    /// `nixvm run -p PATH`: copy base to PATH, retain on exit.
    fn persistent(base: &Path, dest: PathBuf) -> Result<Self> {
        if dest.exists() {
            bail!(
                "{} already exists; pass `nixvm load {}` to resume it",
                dest.display(),
                dest.display(),
            );
        }
        copy_writable(base, &dest)?;
        Ok(Self {
            path: dest,
            mode: OverlayMode::Persistent,
        })
    }

    /// `nixvm load PATH`: open PATH in place. Writes mutate it.
    fn load(path: PathBuf) -> Result<Self> {
        if !path.exists() {
            bail!("{} does not exist", path.display());
        }
        let perm_mode = fs::metadata(&path)?.permissions().mode();
        if perm_mode & 0o200 == 0 {
            bail!(
                "{} is not writable; chmod u+w it or copy it elsewhere first",
                path.display(),
            );
        }
        Ok(Self {
            path,
            mode: OverlayMode::Loaded,
        })
    }
}

impl Drop for Overlay {
    fn drop(&mut self) {
        if matches!(self.mode, OverlayMode::Ephemeral) {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// Copy a (potentially read-only, e.g. /nix/store) base image to `dest`,
/// then chmod the destination 0600 so libkrun can open it read-write.
fn copy_writable(base: &Path, dest: &Path) -> Result<()> {
    fs::copy(base, dest)
        .with_context(|| format!("copy {} → {}", base.display(), dest.display()))?;
    let mut perms = fs::metadata(dest)?.permissions();
    perms.set_mode(0o600);
    fs::set_permissions(dest, perms)?;
    Ok(())
}

// ──────────────────────────── raw terminal ────────────────────────────

/// Best-effort raw mode on the host TTY (fd 0). libkrun would also do this
/// later, but doing it eagerly avoids the kernel line discipline buffering
/// keystrokes during the window between fork and start_enter.
struct RawTerminal {
    /// Dup of the TTY captured at entry. The parent later dup2's fd 0 to
    /// /dev/null so the child is the sole stdin reader; tcsetattr on that
    /// would silently fail with ENOTTY and leave ISIG off (no ^C).
    tty: Option<OwnedFd>,
    saved: Option<rustix::termios::Termios>,
}

impl RawTerminal {
    fn enter() -> Self {
        let stdin = std::io::stdin();
        if !rustix::termios::isatty(&stdin) {
            return Self {
                tty: None,
                saved: None,
            };
        }
        let tty = match stdin.as_fd().try_clone_to_owned() {
            Ok(fd) => fd,
            Err(_) => {
                return Self {
                    tty: None,
                    saved: None,
                };
            }
        };
        let saved = match rustix::termios::tcgetattr(&tty) {
            Ok(t) => t,
            Err(_) => {
                return Self {
                    tty: None,
                    saved: None,
                };
            }
        };
        let mut raw = saved.clone();
        raw.make_raw();
        let _ = rustix::termios::tcsetattr(&tty, rustix::termios::OptionalActions::Now, &raw);
        Self {
            tty: Some(tty),
            saved: Some(saved),
        }
    }
}

impl Drop for RawTerminal {
    fn drop(&mut self) {
        if let (Some(tty), Some(saved)) = (&self.tty, &self.saved) {
            let _ = rustix::termios::tcsetattr(tty, rustix::termios::OptionalActions::Now, saved);
        }
    }
}

// ─────────────────────────────── vmnet ────────────────────────────────

/// In-process bridge between vmnet.framework and a unix datagram socket
/// that libkrun consumes via `krun_add_net_unixgram`. The guest sees a
/// real L2 NIC with ICMP, DHCP, etc.
///
/// Lifetime: created in the parent before `fork()`. Vmnet's `interface_ref`
/// and dispatch queue are valid only in the creating process, so all
/// vmnet calls and the pump thread stay in the parent. The child inherits
/// `libkrun_fd` via fork and uses it through libkrun.
struct Vmnet {
    iface: sys::interface_ref,
    queue: sys::dispatch_queue_t,
    pump: Option<JoinHandle<()>>,
    pump_fd: Option<OwnedFd>,
    pub libkrun_fd: OwnedFd,
    pub mac: [u8; 6],
    pub mtu: u16,
    pub max_packet_size: usize,
}

impl Vmnet {
    fn start() -> Result<Self> {
        // 1. socketpair(AF_UNIX, SOCK_DGRAM): one end for our pump, the
        //    other for libkrun (inherited via fork).
        let (pump_fd, libkrun_fd) = socketpair_dgram()?;

        // 2. Serial dispatch queue for vmnet's start callback + event callback.
        let label = c"nixvm.vmnet";
        let queue = unsafe { sys::dispatch_queue_create(label.as_ptr(), ptr::null_mut()) };
        if queue.is_null() {
            bail!("dispatch_queue_create returned NULL");
        }

        // 3. Build the vmnet config xpc dict: SHARED mode (NAT to the host's
        //    network, with vmnet's built-in DHCP server). Keys come from
        //    libvmnet at runtime; we read the C-string symbols.
        let desc = unsafe { sys::xpc_dictionary_create(ptr::null(), ptr::null(), 0) };
        if desc.is_null() {
            bail!("xpc_dictionary_create returned NULL");
        }
        unsafe {
            sys::xpc_dictionary_set_uint64(
                desc,
                sys::vmnet_operation_mode_key,
                sys::VMNET_SHARED_MODE as u64,
            );
        }

        // 4. vmnet_start_interface is async; the callback delivers MAC + MTU
        //    + max_packet_size on the dispatch queue. Synchronize with a
        //    Mutex+Condvar: the calling thread (this one) waits, the queue
        //    fires the block which fills the slot and signals.
        type StartResult = Result<StartParams, sys::vmnet_return_t>;
        let slot: Arc<(Mutex<Option<StartResult>>, Condvar)> =
            Arc::new((Mutex::new(None), Condvar::new()));
        let slot_for_block = slot.clone();

        let start_block = block2::RcBlock::new(
            move |status: sys::vmnet_return_t, params: sys::xpc_object_t| {
                let result = if status != sys::VMNET_SUCCESS {
                    Err(status)
                } else {
                    Ok(parse_start_params(params))
                };
                let (lock, cv) = &*slot_for_block;
                *lock.lock().unwrap() = Some(result);
                cv.notify_all();
            },
        );

        let iface = unsafe {
            sys::vmnet_start_interface(
                desc,
                queue,
                // block2's RcBlock derefs to the raw block pointer that
                // matches the Apple "block" ABI vmnet expects.
                &*start_block as *const _ as *mut _,
            )
        };
        unsafe { sys::xpc_release(desc) };
        if iface.is_null() {
            unsafe { sys::dispatch_release(sys::dispatch_object_t { _dq: queue }) };
            bail!("vmnet_start_interface returned NULL");
        }

        // Wait for the start callback.
        let (lock, cv) = &*slot;
        let mut guard = lock.lock().unwrap();
        while guard.is_none() {
            guard = cv.wait(guard).unwrap();
        }
        let params = match guard.take().unwrap() {
            Ok(p) => p,
            Err(status) => {
                unsafe { sys::dispatch_release(sys::dispatch_object_t { _dq: queue }) };
                bail!(
                    "vmnet_start_interface failed: status {status} \
                     (macOS 26+ is required for unprivileged vmnet)"
                );
            }
        };
        drop(guard);

        // 5. Event callback: when vmnet has packets ready, drain them and
        //    push to the unix socket. This block lives in `iface` and runs
        //    on our serial queue; we keep it alive by leaking an Arc onto
        //    the heap (block2 retains the closure).
        let pump_send_fd = pump_fd.as_raw_fd();
        let max_pkt = params.max_packet_size;
        let event_block = block2::RcBlock::new(
            move |_event_id: sys::interface_event_t, _event: sys::xpc_object_t| {
                drain_vmnet_to_socket(iface, pump_send_fd, max_pkt);
            },
        );
        let cb_status = unsafe {
            sys::vmnet_interface_set_event_callback(
                iface,
                sys::VMNET_INTERFACE_PACKETS_AVAILABLE,
                queue,
                &*event_block as *const _ as *mut _,
            )
        };
        if cb_status != sys::VMNET_SUCCESS {
            unsafe {
                stop_vmnet_blocking(iface, queue);
                sys::dispatch_release(sys::dispatch_object_t { _dq: queue });
            }
            bail!("vmnet_interface_set_event_callback failed: {cb_status}");
        }
        // event_block must outlive the interface — vmnet retains the block via
        // copy semantics, so dropping our RcBlock is safe.
        drop(event_block);

        // 6. Spawn the socket→vmnet pump. Send the interface pointer as
        // a usize so the closure has a Send capture; reconstitute on the
        // other side. (vmnet APIs are documented as thread-safe.)
        let pump_owned = pump_fd;
        let pump_recv_fd = pump_owned.as_raw_fd();
        let iface_addr = iface as usize;
        let pump = std::thread::Builder::new()
            .name("nixvm.netpump".into())
            .spawn(move || {
                let iface = iface_addr as sys::interface_ref;
                pump_socket_to_vmnet(pump_recv_fd, iface, max_pkt)
            })
            .context("spawn vmnet pump thread")?;

        Ok(Self {
            iface,
            queue,
            pump: Some(pump),
            pump_fd: Some(pump_owned),
            libkrun_fd,
            mac: params.mac,
            mtu: params.mtu,
            max_packet_size: params.max_packet_size,
        })
    }
}

impl Drop for Vmnet {
    fn drop(&mut self) {
        // Closing pump_fd makes the libkrun child's writes go to a closed
        // peer (silently dropped, fine because the VM is exiting), and our
        // pump thread's recv() returns 0 → exits cleanly.
        self.pump_fd.take();
        if let Some(handle) = self.pump.take() {
            let _ = handle.join();
        }
        // vmnet_stop_interface is async; block until completion.
        unsafe {
            stop_vmnet_blocking(self.iface, self.queue);
            sys::dispatch_release(sys::dispatch_object_t { _dq: self.queue });
        }
    }
}

unsafe fn stop_vmnet_blocking(iface: sys::interface_ref, queue: sys::dispatch_queue_t) {
    let done: Arc<(Mutex<bool>, Condvar)> = Arc::new((Mutex::new(false), Condvar::new()));
    let done_for_block = done.clone();
    let stop_block = block2::RcBlock::new(move |_status: sys::vmnet_return_t| {
        let (lock, cv) = &*done_for_block;
        *lock.lock().unwrap() = true;
        cv.notify_all();
    });
    let _ = unsafe { sys::vmnet_stop_interface(iface, queue, &*stop_block as *const _ as *mut _) };
    let (lock, cv) = &*done;
    let mut guard = lock.lock().unwrap();
    while !*guard {
        guard = cv.wait(guard).unwrap();
    }
}

struct StartParams {
    mac: [u8; 6],
    mtu: u16,
    max_packet_size: usize,
}

fn parse_start_params(params: sys::xpc_object_t) -> StartParams {
    // MAC arrives as "aa:bb:cc:dd:ee:ff".
    let mac_cstr = unsafe { sys::xpc_dictionary_get_string(params, sys::vmnet_mac_address_key) };
    let mac = if !mac_cstr.is_null() {
        parse_mac(
            unsafe { CStr::from_ptr(mac_cstr) }
                .to_string_lossy()
                .as_ref(),
        )
    } else {
        [0; 6]
    };
    let mtu = unsafe { sys::xpc_dictionary_get_uint64(params, sys::vmnet_mtu_key) } as u16;
    let max_packet_size =
        unsafe { sys::xpc_dictionary_get_uint64(params, sys::vmnet_max_packet_size_key) } as usize;
    StartParams {
        mac,
        mtu,
        max_packet_size,
    }
}

fn parse_mac(s: &str) -> [u8; 6] {
    let mut out = [0u8; 6];
    for (i, byte) in s.split(':').take(6).enumerate() {
        out[i] = u8::from_str_radix(byte, 16).unwrap_or(0);
    }
    out
}

fn socketpair_dgram() -> Result<(OwnedFd, OwnedFd)> {
    let mut fds: [c_int; 2] = [-1; 2];
    let rc = unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_DGRAM, 0, fds.as_mut_ptr()) };
    if rc != 0 {
        bail!("socketpair: {}", std::io::Error::last_os_error());
    }
    // Bigger SO_SNDBUF/SO_RCVBUF so a flurry of packets doesn't ENOBUFS.
    let want: c_int = 4 * 1024 * 1024;
    for &fd in &fds {
        unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_SNDBUF,
                &want as *const _ as *const c_void,
                std::mem::size_of::<c_int>() as libc::socklen_t,
            );
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_RCVBUF,
                &want as *const _ as *const c_void,
                std::mem::size_of::<c_int>() as libc::socklen_t,
            );
        }
    }
    let pump = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    let libkrun = unsafe { OwnedFd::from_raw_fd(fds[1]) };
    Ok((pump, libkrun))
}

/// Drain everything currently sitting in vmnet into the pump socket. Called
/// from the vmnet event callback (on the dispatch queue).
fn drain_vmnet_to_socket(iface: sys::interface_ref, fd: RawFd, max_pkt: usize) {
    loop {
        // Issue one vmnet_read for one packet; loop until we'd block.
        let mut buf = vec![0u8; max_pkt];
        let mut iov = libc::iovec {
            iov_base: buf.as_mut_ptr() as *mut c_void,
            iov_len: max_pkt,
        };
        let mut pkt = sys::vmpktdesc {
            vm_pkt_size: max_pkt,
            vm_pkt_iov: &mut iov as *mut _ as *mut sys::iovec,
            vm_pkt_iovcnt: 1,
            vm_flags: 0,
        };
        let mut count: c_int = 1;
        let status = unsafe { sys::vmnet_read(iface, &mut pkt, &mut count) };
        if status != sys::VMNET_SUCCESS || count < 1 {
            return;
        }
        // pkt.vm_pkt_size now holds the actual packet length.
        let n = unsafe { libc::write(fd, buf.as_ptr() as *const c_void, pkt.vm_pkt_size) };
        if n < 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() != Some(libc::EAGAIN) {
                warn!(error = %err, "vmnet→socket write failed");
            }
            return;
        }
    }
}

/// Read packets from the pump socket and push them into vmnet. Runs on a
/// dedicated thread until `recv` returns 0 (libkrun closed its end).
fn pump_socket_to_vmnet(fd: RawFd, iface: sys::interface_ref, max_pkt: usize) {
    let mut buf = vec![0u8; max_pkt];
    loop {
        let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut c_void, max_pkt) };
        if n <= 0 {
            return; // EOF or error → exit
        }
        let n = n as usize;
        let mut iov = libc::iovec {
            iov_base: buf.as_mut_ptr() as *mut c_void,
            iov_len: n,
        };
        let mut pkt = sys::vmpktdesc {
            vm_pkt_size: n,
            vm_pkt_iov: &mut iov as *mut _ as *mut sys::iovec,
            vm_pkt_iovcnt: 1,
            vm_flags: 0,
        };
        let mut count: c_int = 1;
        let status = unsafe { sys::vmnet_write(iface, &mut pkt, &mut count) };
        if status != sys::VMNET_SUCCESS {
            warn!(status, "vmnet_write failed; dropping packet");
        }
    }
}

// Re-export for the OwnedFd construction above.
use std::os::fd::FromRawFd;

// ─────────────────────────── libkrun + fork ───────────────────────────

fn fork_and_run_vm(overlay: &Overlay, vmnet: &Vmnet, cpus: u8, mem_mib: u32) -> Result<u8> {
    // We deliberately do NOT call krun_create_ctx in the parent: libkrun's
    // krun_start_enter() never returns and exit()s the process. Doing all
    // libkrun calls in the child means the parent retains its identity for
    // cleanup (overlay unlink, TTY restore) and for surfacing exit status.
    let net_fd = vmnet.libkrun_fd.as_raw_fd();
    let net_mac = vmnet.mac;
    let net_mtu = vmnet.mtu;

    let pid = unsafe { libc::fork() };
    if pid < 0 {
        bail!("fork: {}", std::io::Error::last_os_error());
    }

    if pid == 0 {
        // Child: configure libkrun and start the VM. krun_start_enter() does
        // not return; on failure we _exit() with a distinguishable code so
        // the parent can surface it.
        if let Err(err) = configure_and_start_vm(
            overlay.path.as_path(),
            cpus,
            mem_mib,
            net_fd,
            net_mac,
            net_mtu,
        ) {
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
        warn!(signal = sig, "VM terminated by signal");
        Ok(128 + sig as u8)
    } else {
        bail!("VM exited with unknown status {status}")
    }
}

fn configure_and_start_vm(
    overlay_path: &Path,
    cpus: u8,
    mem_mib: u32,
    net_fd: RawFd,
    net_mac: [u8; 6],
    net_mtu: u16,
) -> Result<()> {
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

    krun_check(
        unsafe { krun_set_vm_config(ctx, cpus, mem_mib) },
        "krun_set_vm_config",
    )?;

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

    // virtio-net wired to the FD inherited from the parent. The parent's
    // `Vmnet` pump bridges this socket to vmnet.framework, giving the
    // guest a real L2 NIC (with ICMP).
    debug!(net_fd, ?net_mac, net_mtu, "krun_add_net_unixgram");
    let mut mac = net_mac;
    krun_check(
        unsafe {
            krun_add_net_unixgram(
                ctx,
                ptr::null(),
                net_fd,
                mac.as_mut_ptr(),
                /* features (0 = libkrun default) */ 0,
                /* flags  (NET_FLAG_VFKIT is gvproxy-specific, not us) */ 0,
            )
        },
        "krun_add_net_unixgram",
    )?;
    let _ = net_mtu; // libkrun derives MTU from virtio negotiation; param reserved for future API.

    // No krun_add_virtio_console_default: libkrun's *implicit* console
    // already wires guest /dev/hvc0 to the host's fd 0/1/2 when those are
    // a TTY (see autoconfigure_console_ports in libkrun). Adding an
    // explicit console on top creates a second virtio-console port, which
    // appeared to corrupt input. Same pattern libkrun's chroot_vm.c uses.

    // We don't use a vsock device, but libkrun creates one implicitly.
    // Disable it to stop the vsock muxer logging unexpected packets.
    debug!("krun_disable_implicit_vsock");
    krun_check(
        unsafe { krun_disable_implicit_vsock(ctx) },
        "krun_disable_implicit_vsock",
    )?;

    // Hand control to libkrun. Does not return on success.
    debug!("krun_start_enter");
    krun_check(unsafe { krun_start_enter(ctx) }, "krun_start_enter")?;
    Ok(())
}

fn krun_check(rc: i32, what: &str) -> Result<()> {
    if rc == 0 {
        Ok(())
    } else {
        Err(anyhow!(
            "{what} failed: {rc} ({})",
            std::io::Error::from_raw_os_error(-rc)
        ))
    }
}
