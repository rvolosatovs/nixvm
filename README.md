# nixvm

Launch a Nix flake output as an ephemeral, headless Linux VM on macOS via
[libkrun](https://github.com/containers/libkrun) — your terminal becomes the
guest's `/dev/hvc0`, no SSH involved.

```
nixvm github:user/repo#some-vm-image
```

PoC. macOS Apple Silicon only. Headless only. Nix store shared read-only
from the host via virtio-fs. State is ephemeral — a fresh per-launch overlay
of the built image, deleted on exit.

## How it works

```
nixvm
  ├── parses the flake ref
  ├── evaluates and realises the output via the upstream Nix C API
  ├── copies the realised raw EFI image to a tempfile (the per-launch overlay)
  ├── puts the host TTY in raw mode
  └── fork() →
       └─ child: configures libkrun (root disk + virtiofs nixstore + virtio
                console wired to fd 0/1/2) and calls krun_start_enter(),
                which never returns.
       parent waits, restores the TTY, unlinks the overlay.
```

Networking is libkrun's built-in TSI (transparent socket impersonation):
outbound TCP/UDP from the guest transparently uses the host's network stack.
No `ping` (no ICMP), no inbound connections — those are out of scope for the PoC.

## Flake-output contract

`<flake>#<attr-path>` must evaluate to a derivation that builds a raw EFI
disk image bootable under libkrun. Concretely, the image must satisfy:

- Raw format (not qcow2) with a GPT partition table and an EFI System Partition.
- Kernel parameters include `console=hvc0`.
- Initrd contains the modules `virtio_pci`, `virtio_blk`, `virtio_console`, `virtiofs`.
- Mounts the host's `/nix/store` from virtiofs with mount tag `nixstore`,
  read-only, marked `neededForBoot`.
- Has a getty (or auto-login) on `hvc0` so the user lands in a shell on boot.

`examples/test-vm/module.nix` is a reference NixOS module satisfying all of
these — copy it or import it into your own flake.

## Build

You need a working Nix install (with the C API — Nix ≥ 2.30) and macOS 14+.

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
nix build path:./examples/test-vm#packages.aarch64-linux.default

# launch it
./target/release/nixvm path:./examples/test-vm#packages.aarch64-linux.default
```

Press `Ctrl+D` or run `poweroff` inside the guest to exit. `Ctrl+C` is
forwarded to the guest as SIGINT (the host TTY is in raw mode).

### Linux builder caveat (Determinate Nix users)

`make-disk-image.nix` (used by `examples/test-vm`) fails inside Determinate
Nix's `external-builders` VM with `chmod: changing permissions of '/build':
Operation not permitted` — the builder's sandbox blocks the privileged ops
that disk image construction needs. Workarounds:

- Build with a remote Linux machine (configure a real `builders =` entry
  pointing at one).
- Run the image build natively on Linux and copy the result into the local
  store.
- Replace the `examples/test-vm` image-build derivation with one that
  doesn't need privileged operations (e.g. systemd-repart / UKI-based
  approaches that produce raw images in pure Nix).

The nixvm binary itself works correctly given any flake output that
realises to a libkrun-bootable raw EFI image — verified by smoke-testing
the eval+realise path against `github:NixOS/nixpkgs#hello`. The example
flake is the only piece blocked on the builder issue.

## Out of scope (for now)

- Read-write `/nix/store` (planned: OverlayFS in the guest with a tmpfs upper)
- Persistent volumes (no `--volume` flag yet)
- Networking beyond TSI (no real NIC, no ICMP, no inbound — vmnet-helper later)
- GUI / Wayland (planned: Weston-RDP in the guest, GPU accel via Vz/MoltenVK)
- GPU acceleration
- x86_64-darwin host
- Packaging (homebrew, nix flake output for nixvm itself)

## Layout

```
.
├── flake.nix                  # dev shell only; no nixvm packaging
├── Makefile                   # libkrun build + cargo build orchestration
├── build.rs                   # libkrun pkg-config probe + Nix C API bindgen
├── Cargo.toml
├── src/
│   ├── main.rs                # clap CLI
│   └── lib.rs                 # everything: Nix eval/realise, libkrun, fork
├── examples/test-vm/          # reference NixOS image fitting the contract
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
