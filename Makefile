# Top-level orchestration: build libkrun (vendored submodule) into a local
# prefix, then run `cargo build` with PKG_CONFIG_PATH pointing at it.
#
# Use from inside `nix-shell`. Targets:
#   make            -> release nixvm binary
#   make debug      -> debug nixvm binary
#   make libkrun    -> just build + install libkrun into ./build/prefix
#   make install    -> cargo install --path . + codesign installed binary
#   make clean      -> remove ./build and cargo target
#   make distclean  -> also clean libkrun submodule

ROOT      := $(CURDIR)
PREFIX    := $(ROOT)/build/prefix
PKG_CONF  := $(PREFIX)/lib/pkgconfig
LIBKRUN   := vendor/libkrun
LIBKRUN_PC := $(PKG_CONF)/libkrun.pc

CARGO_ENV := PKG_CONFIG_PATH=$(PKG_CONF):$$PKG_CONFIG_PATH \
             LIBRARY_PATH=$(PREFIX)/lib:$$LIBRARY_PATH \
             DYLD_FALLBACK_LIBRARY_PATH=$(PREFIX)/lib:$$DYLD_FALLBACK_LIBRARY_PATH

ENTITLEMENTS := entitlements.plist

.PHONY: all debug libkrun install clean distclean

all: $(LIBKRUN_PC)
	$(CARGO_ENV) cargo build --release
	# libkrun calls Hypervisor.framework, which requires the binary to be
	# codesigned with `com.apple.security.hypervisor`. Ad-hoc sign in place.
	codesign --force --sign - --entitlements $(ENTITLEMENTS) target/release/nixvm

debug: $(LIBKRUN_PC)
	$(CARGO_ENV) cargo build
	codesign --force --sign - --entitlements $(ENTITLEMENTS) target/debug/nixvm

libkrun: $(LIBKRUN_PC)

install: $(LIBKRUN_PC)
	$(CARGO_ENV) cargo install --path . --locked
	# `cargo install` strips the codesignature applied to the cached
	# target/release/nixvm. Re-sign the installed copy so libkrun can call
	# Hypervisor.framework.
	codesign --force --sign - --entitlements $(ENTITLEMENTS) \
	  "$${CARGO_INSTALL_ROOT:-$${CARGO_HOME:-$$HOME/.cargo}}/bin/nixvm"

$(LIBKRUN_PC):
	# Pass PREFIX during build too: libkrun runs install_name_tool at build
	# time (not install time), so the resulting dylib has the install_name
	# baked in. Without PREFIX, it defaults to /usr/local.
	$(MAKE) -C $(LIBKRUN) EFI=1 PREFIX=$(PREFIX)
	$(MAKE) -C $(LIBKRUN) install EFI=1 PREFIX=$(PREFIX)
	# libkrun.pc.in hardcodes `-lkrun` even when built with EFI=1 (which
	# produces libkrun-efi.dylib). Add a `libkrun.dylib` symlink so the
	# linker resolves `-lkrun` to the EFI variant; leaves the .pc untouched.
	ln -sf libkrun-efi.dylib $(PREFIX)/lib/libkrun.dylib

clean:
	cargo clean
	# libkrun's krun-vmm build.rs bakes CARGO_MANIFEST_DIR into a
	# rustc-env, and cargo caches build-script output. Without this,
	# moving the repo leaves a stale absolute path in the cached output.
	$(MAKE) -C $(LIBKRUN) clean || true
	rm -rf build

distclean: clean
	$(MAKE) -C $(LIBKRUN) clean-all || true
