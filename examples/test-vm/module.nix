# NixOS module satisfying the conventions nixvm requires.
#
# Builds an "appliance" raw GPT disk image using `image.repart` (systemd-
# repart, entirely userspace — no nested QEMU, works inside Determinate
# Nix's `external-builders` VM unlike `make-disk-image.nix`).
#
# Layout:
#   - ESP (vfat) containing a UKI as /EFI/BOOT/BOOTAA64.EFI so libkrun's
#     EDK2 firmware boots it directly with no bootloader install step.
#   - Root partition (ext4, label "nixos") with the NixOS system closure.
#
# Conventions wired here:
#   - console=hvc0 → guest's virtio console (host stdio)
#   - virtio modules in initrd
#   - /nix/store mounted from virtiofs (tag "nixstore", read-only)
#   - autologin on hvc0
{
  config,
  lib,
  modulesPath,
  pkgs,
  ...
}:

let
  imageId = "nixvm-test";
  imageVersion = "1";
in
{
  imports = [ "${modulesPath}/image/repart.nix" ];

  system.image.id = imageId;
  system.image.version = imageVersion;

  # ---- Boot ---------------------------------------------------------------

  # No bootloader: the EDK2 firmware in libkrun boots the UKI directly
  # via the default EFI fallback path (\EFI\BOOT\BOOT<arch>.EFI).
  boot.loader.grub.enable = false;
  boot.loader.systemd-boot.enable = false;
  boot.loader.efi.canTouchEfiVariables = false;

  # libkrun's implicit virtio-console is auto-wired to the host's fd 0/1/2.
  boot.kernelParams = [ "console=hvc0" ];

  boot.initrd.availableKernelModules = [
    "virtio_pci"
    "virtio_blk"
    "virtio_console"
    "virtio_net"
    "virtiofs"
  ];

  # ---- Filesystems --------------------------------------------------------
  fileSystems."/" = {
    device = "/dev/disk/by-partlabel/nixos";
    fsType = "ext4";
  };

  # /nix/store shared from the host via virtio-fs (tag "nixstore",
  # read-only — RW overlay is a future feature).
  fileSystems."/nix/store" = {
    device = "nixstore";
    fsType = "virtiofs";
    options = [ "ro" "nofail" ];
    neededForBoot = true;
  };

  # ---- Login --------------------------------------------------------------
  services.getty.autologinUser = "root";
  # Tell agetty to set TERM=xterm-256color so readline/bash use modern
  # escape sequences that match most host terminals (kitty, iTerm,
  # Terminal.app). environment.sessionVariables only kicks in after login
  # — agetty's environment is what the shell inherits.
  services.getty.extraArgs = [ "--keep-baud" "--noclear" ];
  systemd.services."getty@hvc0".environment.TERM = "xterm-256color";
  users.users.root.initialHashedPassword = "";
  users.users.root.shell = pkgs.bashInteractive;

  # ---- Networking ---------------------------------------------------------
  # nixvm wires a virtio-net device backed by Apple's vmnet.framework. The
  # framework includes a DHCP server, so just enable the client.
  networking.useDHCP = true;
  networking.firewall.enable = false; # PoC; lock down later if needed.

  # ---- Image build (systemd-repart, no runInLinuxVM) ---------------------
  image.repart.name = imageId;

  # ESP: a vfat partition holding the UKI at the EFI fallback path so
  # libkrun's EDK2 picks it up automatically without a bootloader install.
  image.repart.partitions."10-esp".contents."/EFI/BOOT/BOOT${lib.toUpper pkgs.stdenv.hostPlatform.efiArch}.EFI".source =
    "${config.system.build.uki}/${config.system.boot.loader.ukiFile}";
  image.repart.partitions."10-esp".repartConfig.Type = "esp";
  image.repart.partitions."10-esp".repartConfig.Format = "vfat";
  # NixOS aarch64 UKIs hover around 70 MiB; 256M leaves headroom and keeps
  # the whole image comfortably inside the determinate-nixd builder's tmpfs.
  image.repart.partitions."10-esp".repartConfig.SizeMinBytes = "256M";
  image.repart.partitions."10-esp".repartConfig.SizeMaxBytes = "256M";

  # Root: tiny ext4 — we don't need to ship the closure here because
  # /nix/store comes from the host via virtiofs. Stage 1 init lives in
  # the UKI's initrd; once it mounts /nix/store, the system closure is
  # reachable.
  image.repart.partitions."20-root".repartConfig.Type = "root";
  image.repart.partitions."20-root".repartConfig.Format = "ext4";
  image.repart.partitions."20-root".repartConfig.Label = "nixos";
  image.repart.partitions."20-root".repartConfig.SizeMinBytes = "32M";
  image.repart.partitions."20-root".repartConfig.SizeMaxBytes = "32M";

  system.stateVersion = "25.11";
  networking.hostName = "nixvm";

  documentation.enable = lib.mkDefault false;
  documentation.man.enable = lib.mkDefault false;
}
