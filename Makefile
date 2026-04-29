# Top-level orchestration: build libkrun (vendored submodule) into a local
# prefix, then run `cargo build` with PKG_CONFIG_PATH pointing at it.
#
# Use from inside `nix-shell`. Targets:
#   make            -> release nixvm binary
#   make debug      -> debug nixvm binary
#   make libkrun    -> just build + install libkrun into ./build/prefix
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

ENTITLEMENTS := $(LIBKRUN)/hvf-entitlements.plist

.PHONY: all debug libkrun clean distclean

all: $(LIBKRUN_PC)
	$(CARGO_ENV) cargo build --release
	# libkrun calls Hypervisor.framework, which requires the binary to be
	# codesigned with `com.apple.security.hypervisor`. Ad-hoc sign in place.
	codesign --sign - --entitlements $(ENTITLEMENTS) target/release/nixvm

debug: $(LIBKRUN_PC)
	$(CARGO_ENV) cargo build
	codesign --sign - --entitlements $(ENTITLEMENTS) target/debug/nixvm

libkrun: $(LIBKRUN_PC)

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
	rm -rf build

distclean: clean
	$(MAKE) -C $(LIBKRUN) clean-all || true
