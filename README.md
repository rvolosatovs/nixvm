# nixvm

Launch a Nix flake output as an ephemeral, headless Linux VM on macOS via
[libkrun](https://github.com/containers/libkrun) — your terminal becomes the
guest's `/dev/hvc0`, no SSH involved.

```
nixvm github:user/repo#some-vm-image
```

PoC. **macOS 26+ Apple Silicon only.** Headless only. Real virtio-net
(via Apple `vmnet.framework`, in-process) — `ping`, DNS, outbound
TCP/UDP all work. Nix store shared read-only from the host via
virtio-fs. State is ephemeral — a fresh per-launch overlay of the built
image, deleted on exit.

> **Note:** this project is mostly AI-generated.

## How it works

```
nixvm
  ├── parses the flake ref
  ├── evaluates and realises the output via the upstream Nix C API
  ├── copies the realised raw EFI image to a tempfile (the per-launch overlay)
  ├── opens vmnet.framework + spawns a packet-pump thread (parent only)
  ├── puts the host TTY in raw mode
  └── fork() →
       └─ child: configures libkrun (root disk + virtiofs nixstore + the
                socketpair fd as virtio-net) and calls krun_start_enter(),
                which never returns.
       parent waits, joins the pump, stops vmnet, restores the TTY,
       unlinks the overlay.
```

**Networking** is a real virtio-net device backed by Apple's
`vmnet.framework`. nixvm opens the vmnet interface in-process (no
`vmnet-helper` subprocess, no `vmnet-broker` daemon), runs a packet pump
on a thread, and hands libkrun the socket end via
`krun_add_net_unixgram`. The guest sees an L2 NIC, vmnet's built-in
DHCP server hands it an IP — `ping`, `traceroute`, ICMP and all
ordinary outbound traffic Just Work. No inbound connections.

## Flake-output contract

`<flake>#<attr-path>` must evaluate to a NixOS configuration whose
`config.system.build` exposes everything nixvm boots from. Concretely:

- A raw single-partition root-fs image at `system.build.image` /
  `image.fileName` (no GPT/ESP/UKI — nixvm boots the kernel directly via
  `krun_set_kernel`).
- `system.build.toplevel` staging `kernel`, `initrd`, `init`, and
  `kernel-params` at the conventional paths inside it (NixOS already
  does this).
- `system.build.closureInfo` exposing a `pkgs.closureInfo`-style
  registration file — pointed to by `regInfo=` on the cmdline so the
  guest can `nix-store --load-db` paths visible via virtiofs.
- Kernel parameters include `console=hvc0`.
- Initrd contains the modules `virtio_pci`, `virtio_blk`, `virtio_console`, `virtio_net`, `virtiofs`.
- Mounts the host's `/nix/store` from virtiofs with mount tag `nixstore`,
  read-only, marked `neededForBoot`, with a tmpfs-upper overlay on top
  so the guest can write the store.
- `boot.nixStoreMountOpts = [ ]` so NixOS doesn't re-bind the store
  read-only after stage-2 (which would shadow the writable overlay).
- Has a getty (or auto-login) on `hvc0` so the user lands in a shell on boot.
- DHCP enabled (e.g. `networking.useDHCP = true;`) so the virtio-net
  interface gets an IP from vmnet's built-in DHCP server.

`nixvm.nixosModules.guest` (in `modules/guest.nix`) wires all of this up
in one place. `examples/minimal/module.nix` shows the minimum on top
(image identity + stateVersion).

By default, the guest module builds a 256M single-partition image. Raise
that for images that need more writable guest state:

```nix
{
  nixvm.guest.rootSize = "4G";
}
```

Out-of-tree configurations can import `nixvm.nixosModules.guest`
directly. For example,
[`nixelium`](https://github.com/rvolosatovs/nixelium) exposes a
nixvm-ready variant of its baseline NixOS module:

```sh
nixvm run --override-input nixvm . github:rvolosatovs/nixelium#aarch64-vm-nixvm
```

(The `--override-input nixvm .` points the upstream flake's `nixvm`
input at this checkout — drop it once you're running a tagged release.)

## Build

You need a working Nix install (with the C API — Nix ≥ 2.30) and **macOS 26+**
(unprivileged `vmnet.framework` use requires it).

```sh
nix develop
make           # builds vendored libkrun then runs `cargo build --release`
```

Output: `target/release/nixvm`.

`make` builds libkrun once and caches it in `build/prefix/`. Repeat
incremental edits with `cargo build --release` directly (with the right
`PKG_CONFIG_PATH` exported — `nix develop` and `make` set it for you).

## Run

```sh
# build the example image (needs a linux builder)
nix build path:./examples/minimal#packages.aarch64-linux.default

# launch it
./target/release/nixvm path:./examples/minimal#packages.aarch64-linux.default
```

Press `Ctrl+D` or run `poweroff` inside the guest to exit. `Ctrl+C` is
forwarded to the guest as SIGINT (the host TTY is in raw mode).

### Image build on Determinate Nix

`examples/minimal` builds with `image.repart` (systemd-repart) instead
of `make-disk-image.nix`, so it works inside Determinate Nix's
`external-builders` VM out of the box (no nested QEMU, no privileged
ops). The image is a single ext4 root partition — nixvm passes the
kernel + initrd + cmdline to libkrun directly via `krun_set_kernel`,
so there's no UKI, no ESP, and no bootloader install step.

## Out of scope (for now)

- Persistent volumes (no `--volume` flag yet)
- Inbound network connections / port forwarding from the host
- macOS < 26 (unprivileged `vmnet` requires 26)
- GUI / Wayland (planned: Weston-RDP in the guest, GPU accel via Vz/MoltenVK)
- GPU acceleration
- x86_64-darwin host
- Packaging (homebrew, nix flake output for nixvm itself)

## Layout

```
.
├── flake.nix                  # dev shell only; no nixvm packaging
├── Makefile                   # libkrun build + cargo build orchestration
├── build.rs                   # libkrun pkg-config + Nix C API + vmnet bindings
├── Cargo.toml
├── entitlements.plist         # codesign entitlements (hypervisor + virtualization)
├── src/
│   ├── main.rs                # clap CLI
│   └── lib.rs                 # Nix eval/realise + libkrun + vmnet pump + fork
├── examples/minimal/          # minimal reference NixOS image fitting the contract
└── vendor/libkrun/            # git submodule (containers/libkrun, pinned)
```

## Limitations & known sharp edges

- Building takes a while on first run because libkrun's macOS build needs to
  download a Debian sysroot to cross-compile its init binary.
- The vendored libkrun's `.pc` file says `-lkrun` even when built with
  `EFI=1` (which produces `libkrun-efi.dylib`); `Makefile` adds a
  `libkrun.dylib → libkrun-efi.dylib` symlink so the linker resolves
  `-lkrun` to the EFI variant.
- The runtime path to `libkrun-efi.dylib` is baked into the binary at build
  time (via `install_name_tool`), so moving the binary or `build/prefix`
  will break things. Rebuild after relocating.
- Networking requires the binary to be ad-hoc codesigned with both
  `com.apple.security.hypervisor` (libkrun) and
  `com.apple.security.virtualization` (vmnet on macOS 26). The Makefile
  does this automatically; if you `cargo build` directly, run
  `codesign --force --sign - --entitlements entitlements.plist target/release/nixvm`
  yourself afterward.
