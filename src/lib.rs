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

// Per-instance fetcher-settings constructor that fixes upstream's
// move-from-temporary segfault. See `c_src/nix_setting_shim.cc`.
unsafe extern "C" {
    fn nixvm_fetchers_settings_new(
        ctx: *mut nix_sys::nix_c_context,
    ) -> *mut nix_sys::nix_fetchers_settings;
}

#[derive(Debug, Clone)]
pub struct RunArgs {
    pub flake_ref: String,
    /// `(key, uri)` pairs from `--override-input KEY URI`, applied during
    /// flake locking. Mirrors `nix build --override-input`.
    pub overrides: Vec<(String, String)>,
    /// `(name, value)` pairs from `--option NAME VALUE`, applied via
    /// `nix_setting_set` before libstore/libexpr init. Mirrors
    /// `nix --option`. Unknown names warn and are skipped rather than
    /// fatal — same behavior as `nix --option <unknown>`.
    pub settings: Vec<(String, String)>,
    /// `--tarball-ttl SECONDS`. Applied via `nix_setting_set` against
    /// `globalConfig` (where libstore registers it). Mirrors
    /// `nix --tarball-ttl`.
    pub tarball_ttl: Option<u32>,
    /// If `Some`, copy the image to this path and keep it across exit.
    /// Resume later with `nixvm load <path>`.
    pub persist: Option<PathBuf>,
    /// Overwrite `persist` if it already exists. Mirrors `--force`/`-f`.
    pub force: bool,
    /// If true, double-fork after setup and run the VM in a detached daemon
    /// owned by launchd. Stdout/stderr go to a per-launch log file under
    /// `$XDG_STATE_HOME/nixvm/logs/`; the original `nixvm` process exits 0
    /// immediately so the user's shell prompt returns.
    pub detach: bool,
    pub cpus: u8,
    pub memory_mib: u32,
}

#[derive(Debug, Clone)]
pub struct LoadArgs {
    /// Existing image to boot in place. Writes are persisted back to it.
    pub path: PathBuf,
    /// Optional flake reference. When `Some`, realise it and boot the
    /// existing image against the new closure (refreshing sidecar + GC
    /// root). When `None`, boot against whatever the sidecar already
    /// records — the original `nixvm run -p` semantics.
    pub flake_ref: Option<String>,
    /// Like `RunArgs::overrides`. Ignored unless `flake_ref` is `Some`.
    pub overrides: Vec<(String, String)>,
    /// Like `RunArgs::settings`. Ignored unless `flake_ref` is `Some`.
    pub settings: Vec<(String, String)>,
    /// Like `RunArgs::tarball_ttl`. Ignored unless `flake_ref` is `Some`.
    pub tarball_ttl: Option<u32>,
    pub detach: bool,
    pub cpus: u8,
    pub memory_mib: u32,
}

/// Output of [`nix_realise_image`]: every host path the launch needs.
#[derive(Debug)]
struct Realised {
    image_file: PathBuf,
    toplevel: PathBuf,
    closure_info: PathBuf,
}

/// Inputs to libkrun's `krun_set_kernel`, derived from a realised toplevel.
#[derive(Debug)]
struct BootInputs {
    kernel: PathBuf,
    initrd: PathBuf,
    cmdline: String,
}

/// Build a flake's image and boot it. Ephemeral overlay unless `persist` is set.
pub fn run(args: RunArgs) -> Result<u8> {
    let id = uuid::Uuid::now_v7();
    info!(%id, "starting");

    debug!(flake = %args.flake_ref, "evaluating + realising flake output");
    let realised = nix_realise_image(
        &args.flake_ref,
        &args.overrides,
        &args.settings,
        args.tarball_ttl,
    )
    .context("failed to evaluate or realise the flake output")?;
    debug!(
        image = %realised.image_file.display(),
        toplevel = %realised.toplevel.display(),
        closure_info = %realised.closure_info.display(),
        "realised",
    );

    let boot = boot_inputs_from_toplevel(&realised.toplevel, &realised.closure_info)
        .context("derive kernel/initrd/cmdline from realised toplevel")?;

    let overlay = match args.persist {
        Some(path) => Overlay::persistent(&realised.image_file, path, args.force),
        None => Overlay::ephemeral(&realised.image_file, id),
    }
    .context("failed to prepare overlay")?;

    // GC-root the closure on the host so a host-side `nix-collect-garbage`
    // can't drop paths the running guest is reading via virtiofs. For
    // persistent images we also write a sidecar so a later `nixvm load`
    // can recover toplevel/closureInfo without re-evaluating the flake.
    let gc_root = match overlay.mode {
        OverlayMode::Persistent => {
            write_sidecar(&overlay.path, &realised.toplevel, &realised.closure_info)
                .context("write image sidecar")?;
            GcRoot::persistent(&realised.closure_info, &overlay.path)
                .context("install persistent GC root")?
        }
        OverlayMode::Ephemeral => {
            GcRoot::ephemeral(&realised.closure_info, id).context("install ephemeral GC root")?
        }
        OverlayMode::Loaded => unreachable!("run does not produce Loaded overlays"),
    };

    if args.detach {
        detach_into_daemon(id).context("detach into daemon")?;
    }

    let result = launch_vm(overlay, &boot, id, args.cpus, args.memory_mib);
    drop(gc_root);
    result
}

/// Boot a previously-saved image (from `nixvm run -p`) in place. Writes
/// during the run mutate the file; resume by running `load` again.
///
/// If `args.flake_ref` is `Some`, the flake is realised and the image is
/// booted against that new closure — the sidecar and GC root are updated
/// to point at it, and the previously-rooted closure becomes host-GC
/// eligible. The image file itself is not touched, so on-disk state
/// (`/var`, `/etc`, `/home`) carries forward. Compatibility constraints
/// match `nixos-rebuild boot` followed by reboot on bare metal: NixOS
/// activation handles the migration on next boot, and a wildly broken
/// closure can leave the image in a bad state (recoverable by re-running
/// `nixvm load PATH <known-good-flake>`).
pub fn load(args: LoadArgs) -> Result<u8> {
    let id = uuid::Uuid::now_v7();
    info!(%id, "starting");

    let overlay = Overlay::load(args.path.clone()).context("failed to open image")?;

    let (toplevel, closure_info) = if let Some(flake_ref) = &args.flake_ref {
        debug!(flake = %flake_ref, "evaluating + realising flake output");
        let realised =
            nix_realise_image(flake_ref, &args.overrides, &args.settings, args.tarball_ttl)
                .context("failed to evaluate or realise the flake output")?;
        debug!(
            toplevel = %realised.toplevel.display(),
            closure_info = %realised.closure_info.display(),
            "realised",
        );
        // Overwrite the sidecar so a later `nixvm load PATH` (no flake)
        // resumes against this newer closure. `nix_realise_image` also
        // builds the disk image derivation as a side effect; we drop
        // `realised.image_file` here because the existing image at
        // `args.path` is the writable state container — the image-build
        // output is unused (and host-GC eligible since we don't root it).
        write_sidecar(&overlay.path, &realised.toplevel, &realised.closure_info)
            .context("update image sidecar")?;
        (realised.toplevel, realised.closure_info)
    } else {
        read_sidecar(&overlay.path)?
    };

    if !toplevel.exists() {
        bail!(
            "{} no longer exists — re-run `nixvm load {} <flake>` to rebuild the closure",
            toplevel.display(),
            args.path.display(),
        );
    }
    if !closure_info.exists() {
        bail!(
            "{} no longer exists — re-run `nixvm load {} <flake>` to rebuild the closure",
            closure_info.display(),
            args.path.display(),
        );
    }

    let boot = boot_inputs_from_toplevel(&toplevel, &closure_info)
        .context("derive kernel/initrd/cmdline from toplevel")?;
    // Refresh the per-image persistent root to point at the (possibly
    // updated) closureInfo. The `nix-store --add-root` indirection is
    // atomic — when the flake changed, the old closure becomes host-GC
    // eligible the moment this returns.
    let gc_root =
        GcRoot::persistent(&closure_info, &overlay.path).context("refresh persistent GC root")?;

    if args.detach {
        detach_into_daemon(id).context("detach into daemon")?;
    }

    let result = launch_vm(overlay, &boot, id, args.cpus, args.memory_mib);
    drop(gc_root);
    result
}

/// Shared launch path: vmnet, raw TTY, fork, libkrun, wait, cleanup.
fn launch_vm(
    overlay: Overlay,
    boot: &BootInputs,
    id: uuid::Uuid,
    cpus: u8,
    mem_mib: u32,
) -> Result<u8> {
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

    let exit_code = fork_and_run_vm(&overlay, boot, &vmnet, cpus, mem_mib)
        .context("failed to launch the VM")?;
    // vmnet drops here, AFTER waitpid → pump thread joins, vmnet_stop_interface fires.
    drop(vmnet);
    drop(overlay);
    Ok(exit_code)
}

// ── boot inputs + sidecar + GC roots ─────────────────────────────────

/// Derive `BootInputs` from a realised `system.build.toplevel`. NixOS
/// stages the artifacts at well-known names inside the toplevel directory
/// (`kernel`, `initrd` are symlinks; `kernel-params` is a flat file with
/// `boot.kernelParams` joined by spaces) — same pattern the UKI build
/// reads from in `nixos/modules/system/boot/uki.nix`.
fn boot_inputs_from_toplevel(toplevel: &Path, closure_info: &Path) -> Result<BootInputs> {
    let kernel = fs::read_link(toplevel.join("kernel"))
        .with_context(|| format!("readlink {}/kernel", toplevel.display()))?;
    let initrd = fs::read_link(toplevel.join("initrd"))
        .with_context(|| format!("readlink {}/initrd", toplevel.display()))?;
    let kernel_params = fs::read_to_string(toplevel.join("kernel-params"))
        .with_context(|| format!("read {}/kernel-params", toplevel.display()))?
        .trim()
        .to_string();
    // Mirrors NixOS UKI cmdline (`init=$toplevel/init <kernelParams>`)
    // plus the `regInfo=` channel qemu-vm.nix uses to deliver the
    // closure registration file path to the guest.
    let mut cmdline = format!(
        "init={}/init {} regInfo={}/registration",
        toplevel.display(),
        kernel_params,
        closure_info.display(),
    );
    // Forward the host's TERM into the guest via systemd's manager
    // environment (parsed from `/proc/cmdline` at PID 1 startup and
    // inherited by every spawned service, including serial-getty@hvc0).
    // Skipped when unset or empty so the guest falls back to whatever
    // agetty defaults to.
    if let Ok(term) = std::env::var("TERM") {
        if !term.is_empty() {
            use std::fmt::Write;
            let _ = write!(cmdline, " systemd.setenv=TERM={term}");
        }
    }
    Ok(BootInputs {
        kernel,
        initrd,
        cmdline,
    })
}

/// `<image>.nixvm` next to a persistent image. Two lines, `key path` each;
/// no JSON to keep the dependency footprint zero.
fn sidecar_path(image: &Path) -> PathBuf {
    let mut p = image.as_os_str().to_owned();
    p.push(".nixvm");
    PathBuf::from(p)
}

fn write_sidecar(image: &Path, toplevel: &Path, closure_info: &Path) -> Result<()> {
    let body = format!(
        "toplevel {}\nclosureInfo {}\n",
        toplevel.display(),
        closure_info.display(),
    );
    let p = sidecar_path(image);
    fs::write(&p, body).with_context(|| format!("write {}", p.display()))
}

fn read_sidecar(image: &Path) -> Result<(PathBuf, PathBuf)> {
    let p = sidecar_path(image);
    let body = fs::read_to_string(&p).with_context(|| {
        format!(
            "read {} (no sidecar — was this image saved by `nixvm run -p`?)",
            p.display()
        )
    })?;
    let mut toplevel: Option<PathBuf> = None;
    let mut closure_info: Option<PathBuf> = None;
    for line in body.lines() {
        let mut it = line.splitn(2, ' ');
        match (it.next(), it.next()) {
            (Some("toplevel"), Some(v)) => toplevel = Some(PathBuf::from(v)),
            (Some("closureInfo"), Some(v)) => closure_info = Some(PathBuf::from(v)),
            _ => {}
        }
    }
    Ok((
        toplevel.ok_or_else(|| anyhow!("{}: missing `toplevel`", p.display()))?,
        closure_info.ok_or_else(|| anyhow!("{}: missing `closureInfo`", p.display()))?,
    ))
}

/// Root directory for nixvm's per-user state (GC root indirections,
/// `--detach` logs, etc.). On macOS this resolves via
/// `dirs::data_local_dir()` to `~/Library/Application Support/nixvm` — the
/// native convention. Falls back to `$TMPDIR` only if neither `$HOME` nor
/// the platform-specific data dir is available; in practice that branch
/// never trips on a normally-configured macOS account.
fn state_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("nixvm")
}

/// Per-launch log file used by `--detach`. One file per launch keyed off
/// the same UUID we already generate for ephemeral GC roots, so a host can
/// have many detached VMs running with non-colliding logs.
fn log_path_for(id: uuid::Uuid) -> PathBuf {
    state_dir().join("logs").join(format!("{id}.log"))
}

/// Classic double-fork daemonize. After this returns in the surviving
/// process: no controlling TTY, parent is launchd, stdin is `/dev/null`,
/// stdout/stderr point at the per-launch log file.
///
/// We reach this point with overlay + GC root already constructed in the
/// foreground (so user-visible errors stay user-visible). Their `Drop`
/// impls live on the stack here; `std::process::exit` in the surviving
/// fork-parents skips destructors, so the ephemeral overlay file and GC
/// root dir aren't unlinked underneath the daemon.
///
/// Caveat: `kill PID` against the daemon will _not_ run those Drops
/// either, leaving an orphaned libkrun child plus stale overlay/GC root.
/// Stop a detached VM by shutting down from inside the guest, or use
/// `pkill -f nixvm` to take the libkrun child down too.
fn detach_into_daemon(id: uuid::Uuid) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;

    let log_path = log_path_for(id);
    if let Some(dir) = log_path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("mkdir -p {}", dir.display()))?;
    }
    eprintln!("nixvm: detached, log: {}", log_path.display());

    // First fork: original process exits so the user's shell prompt returns.
    // The intermediate child does setsid and re-forks; the grandchild is the
    // surviving daemon.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        bail!("first fork: {}", std::io::Error::last_os_error());
    }
    if pid > 0 {
        // Parent: exit without running any Drops on the stack. The daemon
        // owns overlay/GC root cleanup from here.
        std::process::exit(0);
    }

    // setsid detaches from the controlling TTY: SIGHUP from the original
    // shell session can't reach us anymore.
    if unsafe { libc::setsid() } < 0 {
        bail!("setsid: {}", std::io::Error::last_os_error());
    }

    // Second fork so the surviving daemon is not a session leader and can
    // never reacquire a controlling TTY by accident (Stevens APUE §13.3).
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        bail!("second fork: {}", std::io::Error::last_os_error());
    }
    if pid > 0 {
        std::process::exit(0);
    }

    // We're the daemon. Rewire stdio. Tracing was init'd
    // `with_writer(std::io::stderr)`, which resolves to fd 2 on every write
    // — so dup2'ing fd 2 to the log file routes all subsequent tracing
    // output there without re-initialising the subscriber.
    let log = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&log_path)
        .with_context(|| format!("open {}", log_path.display()))?;
    let devnull = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/null")
        .context("open /dev/null")?;
    unsafe {
        if libc::dup2(devnull.as_raw_fd(), 0) < 0 {
            bail!("dup2 stdin: {}", std::io::Error::last_os_error());
        }
        if libc::dup2(log.as_raw_fd(), 1) < 0 {
            bail!("dup2 stdout: {}", std::io::Error::last_os_error());
        }
        if libc::dup2(log.as_raw_fd(), 2) < 0 {
            bail!("dup2 stderr: {}", std::io::Error::last_os_error());
        }
    }
    // log + devnull drop here; their underlying fds close, but fd 0/1/2
    // hold independent dup'd file descriptions and stay open.
    info!(pid = std::process::id(), log = %log_path.display(), "daemonized");
    Ok(())
}

/// Stable per-image directory name for persistent GC roots. Hashes the
/// canonical absolute image path so two `nixvm load PATH` invocations
/// end up at the same GC root, regardless of CWD.
fn persistent_root_dir(image: &Path) -> PathBuf {
    use std::hash::{Hash, Hasher};
    let abs = std::fs::canonicalize(image).unwrap_or_else(|_| image.to_path_buf());
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    abs.hash(&mut hasher);
    state_dir()
        .join("roots")
        .join(format!("{:016x}", hasher.finish()))
}

/// Holds an indirect GC root in `dir` pointing at the realised closureInfo.
/// Rooting closureInfo transitively roots the entire system closure
/// (closureInfo's `references` in the store metadata include every path
/// it lists in `registration`), so `nix-collect-garbage` on the host
/// can't drop paths the running guest reads via virtiofs.
///
/// Ephemeral roots are removed on `Drop`; persistent ones are kept so a
/// later `nixvm load` doesn't have to re-evaluate the flake.
struct GcRoot {
    dir: PathBuf,
    ephemeral: bool,
}

impl GcRoot {
    fn ephemeral(closure_info: &Path, id: uuid::Uuid) -> Result<Self> {
        let dir = state_dir().join("transient").join(id.to_string());
        add_gc_root(closure_info, &dir)?;
        Ok(Self {
            dir,
            ephemeral: true,
        })
    }

    fn persistent(closure_info: &Path, image: &Path) -> Result<Self> {
        let dir = persistent_root_dir(image);
        add_gc_root(closure_info, &dir)?;
        Ok(Self {
            dir,
            ephemeral: false,
        })
    }
}

impl Drop for GcRoot {
    fn drop(&mut self) {
        if self.ephemeral {
            // The user-space symlink disappears; the indirect root in
            // /nix/var/nix/gcroots/auto becomes dangling and is cleaned
            // up by the daemon on the next GC pass.
            let _ = fs::remove_dir_all(&self.dir);
        }
    }
}

fn add_gc_root(closure_info: &Path, dir: &Path) -> Result<()> {
    fs::create_dir_all(dir).with_context(|| format!("mkdir -p {}", dir.display()))?;
    let link = dir.join("closure-info");
    // `nix-store --realise … --add-root LINK --indirect`: realises the
    // path (no-op if already valid), creates LINK as a symlink to the
    // realised /nix/store path, and registers an indirect root in
    // /nix/var/nix/gcroots/auto pointing at LINK. We can't do this
    // through the C API — `nix_store_realise` exists but no GC-root
    // function does, so we shell out.
    let status = std::process::Command::new("nix-store")
        .arg("--realise")
        .arg(closure_info)
        .arg("--add-root")
        .arg(&link)
        .arg("--indirect")
        .stdout(std::process::Stdio::null())
        .status()
        .context("spawn nix-store --add-root")?;
    if !status.success() {
        bail!("nix-store --add-root exited with {status}");
    }
    Ok(())
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

/// Drives the full eval+realise pipeline. Returns every realised store
/// path the launcher needs: the raw disk image, `system.build.toplevel`
/// (kernel/initrd/init/kernel-params live inside it), and the
/// `closureInfo` registration directory.
///
/// Uses the libflake C API end-to-end: parse the user's flake-ref against
/// $PWD as base directory (so `./foo` resolves like `nix build`), apply
/// any `--override-input` entries during virtual locking, then walk the
/// resulting outputs tree to extract drvPath/outPath/fileName.
fn nix_realise_image(
    flake_uri: &str,
    overrides: &[(String, String)],
    settings: &[(String, String)],
    tarball_ttl: Option<u32>,
) -> Result<Realised> {
    let ctx = NixCtx::new()?;

    unsafe {
        nix_sys::nix_libutil_init(ctx.raw);
        ctx.check().context("nix_libutil_init")?;

        // Enable flakes before libstore/libexpr pick up settings.
        let key = CString::new("experimental-features").unwrap();
        let val = CString::new("nix-command flakes").unwrap();
        nix_sys::nix_setting_set(ctx.raw, key.as_ptr(), val.as_ptr());
        ctx.check().context("enable experimental flakes feature")?;

        // User-supplied `--option NAME VALUE`. Applied here, before
        // libstore/libexpr init, against `globalConfig`. Reaches every
        // `Config` registered via `GlobalConfig::Register` — that
        // includes libutil/libstore/libexpr/libflake settings and the
        // global `nix::fetchSettings`. Unknown names warn and are
        // skipped, matching `nix --option <unknown>`.
        for (name, value) in settings {
            let key = CString::new(name.as_str())
                .with_context(|| format!("--option name `{name}` contains a NUL byte"))?;
            let val = CString::new(value.as_str())
                .with_context(|| format!("--option value for `{name}` contains a NUL byte"))?;
            nix_sys::nix_setting_set(ctx.raw, key.as_ptr(), val.as_ptr());
            if let Err(e) = ctx.check() {
                warn!("ignoring `--option {name} {value}`: {e:#}");
            }
        }

        // `tarball-ttl` lives on libstore's `nix::settings` (registered
        // with `globalConfig`), not on `nix::fetchers::Settings`. Route
        // it through the same `nix_setting_set` path as `--option`, but
        // keep the error fatal — the user explicitly asked for it.
        if let Some(ttl) = tarball_ttl {
            let key = CString::new("tarball-ttl").unwrap();
            let val = CString::new(ttl.to_string()).unwrap();
            nix_sys::nix_setting_set(ctx.raw, key.as_ptr(), val.as_ptr());
            ctx.check().context("apply --tarball-ttl")?;
        }

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

    let flake_settings = unsafe { nix_sys::nix_flake_settings_new(ctx.raw) };
    ctx.check().context("nix_flake_settings_new")?;
    if flake_settings.is_null() {
        bail!("nix_flake_settings_new returned NULL");
    }
    let _flake_settings_guard =
        scopeguard(|| unsafe { nix_sys::nix_flake_settings_free(flake_settings) });

    // `nixvm_fetchers_settings_new`, not `nix_fetchers_settings_new`:
    // upstream's version returns a `Settings` whose `_settings` map is
    // populated with dangling pointers (move-from-temporary), and
    // `Config::set` segfaults on first use. See `c_src/nix_setting_shim.cc`.
    let fetch_settings = unsafe { nixvm_fetchers_settings_new(ctx.raw) };
    ctx.check().context("nixvm_fetchers_settings_new")?;
    if fetch_settings.is_null() {
        bail!("nixvm_fetchers_settings_new returned NULL");
    }
    let _fetch_settings_guard =
        scopeguard(|| unsafe { nix_sys::nix_fetchers_settings_free(fetch_settings) });

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

    // Parse-flags' base directory makes nix resolve user-supplied relative
    // paths (e.g. `./examples/minimal`) against $PWD, exactly like `nix build`.
    let parse_flags =
        unsafe { nix_sys::nix_flake_reference_parse_flags_new(ctx.raw, flake_settings) };
    ctx.check().context("nix_flake_reference_parse_flags_new")?;
    if parse_flags.is_null() {
        bail!("nix_flake_reference_parse_flags_new returned NULL");
    }
    let _parse_flags_guard =
        scopeguard(|| unsafe { nix_sys::nix_flake_reference_parse_flags_free(parse_flags) });

    let cwd = std::env::current_dir().context("getcwd")?;
    let cwd_str = cwd.to_str().context("cwd not UTF-8")?;
    let cwd_c = CString::new(cwd_str).unwrap();
    unsafe {
        nix_sys::nix_flake_reference_parse_flags_set_base_directory(
            ctx.raw,
            parse_flags,
            cwd_c.as_ptr(),
            cwd_str.len(),
        );
    }
    ctx.check().context("set base directory on parse flags")?;

    let (flake_ref, fragment) =
        parse_flake_ref(&ctx, fetch_settings, flake_settings, parse_flags, flake_uri)
            .with_context(|| format!("parse flake ref `{flake_uri}`"))?;
    let _flake_ref_guard = scopeguard(|| unsafe { nix_sys::nix_flake_reference_free(flake_ref) });

    // Match the old default: bare flake-ref (no `#`) → `nixosConfigurations.default`.
    let attr_path = if fragment.is_empty() {
        "nixosConfigurations.default".to_string()
    } else {
        fragment
    };

    // Lock flags: virtual mode (lock in memory, never write to disk) plus
    // any `--override-input` entries.
    let lock_flags = unsafe { nix_sys::nix_flake_lock_flags_new(ctx.raw, flake_settings) };
    ctx.check().context("nix_flake_lock_flags_new")?;
    if lock_flags.is_null() {
        bail!("nix_flake_lock_flags_new returned NULL");
    }
    let _lock_flags_guard =
        scopeguard(|| unsafe { nix_sys::nix_flake_lock_flags_free(lock_flags) });

    unsafe { nix_sys::nix_flake_lock_flags_set_mode_virtual(ctx.raw, lock_flags) };
    ctx.check()
        .context("nix_flake_lock_flags_set_mode_virtual")?;

    // Override flake refs must outlive the lock flags struct (which only
    // borrows them). `OverrideRefs` owns them and frees on drop at the end
    // of this function.
    let mut override_refs = OverrideRefs(Vec::new());
    for (key, uri) in overrides {
        let (or_ref, _frag) =
            parse_flake_ref(&ctx, fetch_settings, flake_settings, parse_flags, uri)
                .with_context(|| format!("parse override `{key} {uri}`"))?;
        override_refs.0.push(or_ref);
        let key_c = CString::new(key.as_str()).unwrap();
        unsafe {
            nix_sys::nix_flake_lock_flags_add_input_override(
                ctx.raw,
                lock_flags,
                key_c.as_ptr(),
                or_ref,
            );
        }
        ctx.check()
            .with_context(|| format!("add override `{key} {uri}`"))?;
    }

    let locked = unsafe {
        nix_sys::nix_flake_lock(
            ctx.raw,
            fetch_settings,
            flake_settings,
            state,
            lock_flags,
            flake_ref,
        )
    };
    ctx.check().context("nix_flake_lock")?;
    if locked.is_null() {
        bail!("nix_flake_lock returned NULL");
    }
    let _locked_guard = scopeguard(|| unsafe { nix_sys::nix_locked_flake_free(locked) });

    let outputs = unsafe {
        nix_sys::nix_locked_flake_get_output_attrs(ctx.raw, flake_settings, state, locked)
    };
    ctx.check().context("nix_locked_flake_get_output_attrs")?;
    if outputs.is_null() {
        bail!("nix_locked_flake_get_output_attrs returned NULL");
    }
    let _outputs_guard = scopeguard(|| unsafe {
        let _ = nix_sys::nix_value_decref(ctx.raw, outputs);
    });
    unsafe { nix_sys::nix_value_force(ctx.raw, state, outputs) };
    ctx.check().context("force outputs")?;

    // Walk the outputs tree for every path the launch will need:
    //   - `system.build.image.{drvPath,outPath}` + `image.fileName` →
    //     the raw root-fs image we copy to the overlay.
    //   - `system.build.toplevel.outPath` → host-side directory that
    //     stages `kernel`/`initrd`/`init`/`kernel-params` (read by
    //     [`boot_inputs_from_toplevel`]).
    //   - `system.build.closureInfo.{drvPath,outPath}` → the registration
    //     file pointed to by `regInfo=` on the kernel cmdline.
    let img_drv_path = read_path_string(
        &ctx,
        state,
        outputs,
        &format!("{attr_path}.config.system.build.image.drvPath"),
    )?;
    let img_out_path = read_path_string(
        &ctx,
        state,
        outputs,
        &format!("{attr_path}.config.system.build.image.outPath"),
    )?;
    let file_name = read_path_string(
        &ctx,
        state,
        outputs,
        &format!("{attr_path}.config.image.fileName"),
    )?;
    let toplevel_out = read_path_string(
        &ctx,
        state,
        outputs,
        &format!("{attr_path}.config.system.build.toplevel.outPath"),
    )?;
    let closure_info_drv = read_path_string(
        &ctx,
        state,
        outputs,
        &format!("{attr_path}.config.system.build.closureInfo.drvPath"),
    )?;
    let closure_info_out = read_path_string(
        &ctx,
        state,
        outputs,
        &format!("{attr_path}.config.system.build.closureInfo.outPath"),
    )?;

    // Realise both drvs. Realising closureInfo pulls toplevel in as an
    // input dep, so we get kernel/initrd/init transitively.
    realise_drv(&ctx, store, &img_drv_path).context("realise image drv")?;
    realise_drv(&ctx, store, &closure_info_drv).context("realise closureInfo drv")?;

    Ok(Realised {
        image_file: PathBuf::from(format!("{img_out_path}/{file_name}")),
        toplevel: PathBuf::from(toplevel_out),
        closure_info: PathBuf::from(closure_info_out),
    })
}

fn realise_drv(ctx: &NixCtx, store: *mut nix_sys::Store, drv_path: &str) -> Result<()> {
    let drv_cstr = CString::new(drv_path).unwrap();
    let store_path = unsafe { nix_sys::nix_store_parse_path(ctx.raw, store, drv_cstr.as_ptr()) };
    ctx.check().context("nix_store_parse_path")?;
    if store_path.is_null() {
        bail!("nix_store_parse_path returned NULL for {drv_path}");
    }
    let _g = scopeguard(|| unsafe { nix_sys::nix_store_path_free(store_path) });
    unsafe {
        nix_sys::nix_store_realise(ctx.raw, store, store_path, ptr::null_mut(), None);
    }
    ctx.check().context("nix_store_realise")?;
    Ok(())
}

/// Owning collection of `nix_flake_reference *` pointers used as override
/// inputs; the lock flags borrow them, so they must outlive locking.
struct OverrideRefs(Vec<*mut nix_sys::nix_flake_reference>);
impl Drop for OverrideRefs {
    fn drop(&mut self) {
        for r in &self.0 {
            unsafe { nix_sys::nix_flake_reference_free(*r) };
        }
    }
}

/// Wrap `nix_flake_reference_and_fragment_from_string`. Returns a flake
/// reference (caller must `nix_flake_reference_free`) plus the `#fragment`
/// string (empty if the URI had none).
fn parse_flake_ref(
    ctx: &NixCtx,
    fetch_settings: *mut nix_sys::nix_fetchers_settings,
    flake_settings: *mut nix_sys::nix_flake_settings,
    parse_flags: *mut nix_sys::nix_flake_reference_parse_flags,
    uri: &str,
) -> Result<(*mut nix_sys::nix_flake_reference, String)> {
    let mut fragment_buf: Vec<u8> = Vec::new();
    let mut out: *mut nix_sys::nix_flake_reference = ptr::null_mut();
    let uri_c = CString::new(uri).context("flake URI contains a NUL byte")?;
    unsafe {
        nix_sys::nix_flake_reference_and_fragment_from_string(
            ctx.raw,
            fetch_settings,
            flake_settings,
            parse_flags,
            uri_c.as_ptr(),
            uri.len(),
            &mut out,
            Some(collect_string_cb),
            &mut fragment_buf as *mut _ as *mut c_void,
        );
    }
    ctx.check()
        .with_context(|| format!("nix_flake_reference_and_fragment_from_string({uri})"))?;
    if out.is_null() {
        bail!("nix_flake_reference_and_fragment_from_string returned NULL for `{uri}`");
    }
    let fragment = String::from_utf8(fragment_buf).context("flake fragment not UTF-8")?;
    Ok((out, fragment))
}

/// Walk a dotted attr path under `root` and return a fresh value handle.
/// Caller must `nix_value_decref` the result. Empty / single-segment paths
/// both work; intermediate refs are decref'd as we descend.
fn walk_attrs(
    ctx: &NixCtx,
    state: *mut nix_sys::EvalState,
    root: *mut nix_sys::nix_value,
    path: &str,
) -> Result<*mut nix_sys::nix_value> {
    let segments: Vec<&str> = path.split('.').filter(|s| !s.is_empty()).collect();
    if segments.is_empty() {
        bail!("walk_attrs: empty path");
    }
    let mut cur = root;
    let mut owned: *mut nix_sys::nix_value = ptr::null_mut();
    for seg in segments {
        let cseg = CString::new(seg).unwrap();
        let next = unsafe { nix_sys::nix_get_attr_byname(ctx.raw, cur, state, cseg.as_ptr()) };
        if let Err(err) = ctx
            .check()
            .with_context(|| format!("get `{seg}` while walking `{path}`"))
        {
            if !owned.is_null() {
                unsafe {
                    let _ = nix_sys::nix_value_decref(ctx.raw, owned);
                }
            }
            return Err(err);
        }
        if next.is_null() {
            if !owned.is_null() {
                unsafe {
                    let _ = nix_sys::nix_value_decref(ctx.raw, owned);
                }
            }
            bail!("attribute `{seg}` missing while walking `{path}`");
        }
        unsafe { nix_sys::nix_value_force(ctx.raw, state, next) };
        if let Err(err) = ctx
            .check()
            .with_context(|| format!("force `{seg}` while walking `{path}`"))
        {
            unsafe {
                let _ = nix_sys::nix_value_decref(ctx.raw, next);
            }
            if !owned.is_null() {
                unsafe {
                    let _ = nix_sys::nix_value_decref(ctx.raw, owned);
                }
            }
            return Err(err);
        }
        if !owned.is_null() {
            unsafe {
                let _ = nix_sys::nix_value_decref(ctx.raw, owned);
            }
        }
        owned = next;
        cur = next;
    }
    Ok(owned)
}

/// Walk a dotted attr path and read the string value at the leaf.
fn read_path_string(
    ctx: &NixCtx,
    state: *mut nix_sys::EvalState,
    root: *mut nix_sys::nix_value,
    path: &str,
) -> Result<String> {
    let val = walk_attrs(ctx, state, root, path)?;
    let _g = scopeguard(|| unsafe {
        let _ = nix_sys::nix_value_decref(ctx.raw, val);
    });
    read_string(ctx, |ctx_raw, cb, ud| unsafe {
        nix_sys::nix_get_string(ctx_raw, val, cb, ud)
    })
    .with_context(|| format!("read string at `{path}`"))
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

    /// `nixvm run -p PATH`: copy base to PATH, retain on exit. With
    /// `force`, an existing file at PATH is unlinked first (along with
    /// its sidecar) so the new image replaces it cleanly.
    fn persistent(base: &Path, dest: PathBuf, force: bool) -> Result<Self> {
        if dest.exists() {
            if !force {
                bail!(
                    "{} already exists; pass `nixvm load {}` to resume it, or `--force` to overwrite",
                    dest.display(),
                    dest.display(),
                );
            }
            fs::remove_file(&dest)
                .with_context(|| format!("remove existing {}", dest.display()))?;
            let _ = fs::remove_file(sidecar_path(&dest));
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

fn fork_and_run_vm(
    overlay: &Overlay,
    boot: &BootInputs,
    vmnet: &Vmnet,
    cpus: u8,
    mem_mib: u32,
) -> Result<u8> {
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
            boot,
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
    boot: &BootInputs,
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

    // Direct kernel boot — no UKI, no firmware. NixOS aarch64 emits an
    // uncompressed `Image` (raw kernel) at `system.build.kernel/Image`
    // and a separate initrd at `system.build.initialRamdisk/initrd`;
    // libkrun loads both at the right physical addresses for ARM64 and
    // hands the cmdline to the kernel. KRUN_KERNEL_FORMAT_RAW matches the
    // uncompressed Image format.
    let kernel_path = CString::new(boot.kernel.to_str().context("kernel path not UTF-8")?).unwrap();
    let initrd_path = CString::new(boot.initrd.to_str().context("initrd path not UTF-8")?).unwrap();
    let cmdline = CString::new(boot.cmdline.as_str()).context("cmdline contains NUL byte")?;
    debug!(
        kernel = %boot.kernel.display(),
        initrd = %boot.initrd.display(),
        cmdline = %boot.cmdline,
        "krun_set_kernel",
    );
    krun_check(
        unsafe {
            krun_set_kernel(
                ctx,
                kernel_path.as_ptr(),
                KRUN_KERNEL_FORMAT_RAW,
                initrd_path.as_ptr(),
                cmdline.as_ptr(),
            )
        },
        "krun_set_kernel",
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
