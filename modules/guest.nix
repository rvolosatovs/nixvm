# Shared NixOS module satisfying nixvm's host↔guest contract.
#
# Flake users:
#   imports = [ inputs.nixvm.nixosModules.guest ];
# then add image-specific bits (hostname, stateVersion, extra packages).
#
# Wires the contract end-to-end:
#   - raw GPT image with ESP + UKI at \EFI\BOOT\BOOT<arch>.EFI (no bootloader)
#   - console=hvc0, virtio + overlay modules in initrd
#   - /nix/store overlay: virtiofs from host (ro) ∪ tmpfs (rw) so guest
#     activation (home-manager, nix-env, GC roots) can write the store
#     without the writes escaping back to the host's /nix/store
#   - DHCP client (vmnet's NAT mode is the DHCP server)
#   - autologin on hvc0
{
  config,
  lib,
  modulesPath,
  pkgs,
  ...
}:

{
  imports = [ "${modulesPath}/image/repart.nix" ];

  # ---- Boot ---------------------------------------------------------------
  # No bootloader: libkrun's EDK2 firmware boots the UKI from the EFI
  # fallback path (\EFI\BOOT\BOOT<arch>.EFI).
  boot.loader.grub.enable = lib.mkDefault false;
  boot.loader.systemd-boot.enable = lib.mkDefault false;
  boot.loader.efi.canTouchEfiVariables = lib.mkDefault false;

  # libkrun's implicit virtio-console is auto-wired to the host's fd 0/1/2.
  boot.kernelParams = [ "console=hvc0" ];

  boot.initrd.availableKernelModules = [
    "virtio_pci"
    "virtio_blk"
    "virtio_console"
    "virtio_net"
    "virtiofs"
    "overlay"
  ];

  # ---- Filesystems --------------------------------------------------------
  fileSystems."/".device = "/dev/disk/by-partlabel/nixos";
  fileSystems."/".fsType = "ext4";

  # /nix/store is an overlay so guest writes succeed without escaping back
  # to the host. Lower is virtiofs from the host (ro), upper/work are on a
  # tmpfs — writes are ephemeral and re-created on each boot.
  fileSystems."/nix/.ro-store".device = "nixstore";
  fileSystems."/nix/.ro-store".fsType = "virtiofs";
  fileSystems."/nix/.ro-store".options = [
    "ro"
    "nofail"
  ];
  fileSystems."/nix/.ro-store".neededForBoot = true;

  fileSystems."/nix/.rw-store".fsType = "tmpfs";
  fileSystems."/nix/.rw-store".options = [ "mode=0755" ];
  fileSystems."/nix/.rw-store".neededForBoot = true;

  fileSystems."/nix/store".overlay.lowerdir = [ "/nix/.ro-store" ];
  fileSystems."/nix/store".overlay.upperdir = "/nix/.rw-store/upper";
  fileSystems."/nix/store".overlay.workdir = "/nix/.rw-store/work";
  fileSystems."/nix/store".neededForBoot = true;

  # ---- Login --------------------------------------------------------------
  services.getty.autologinUser = lib.mkDefault "root";
  services.getty.extraArgs = [
    "--keep-baud"
    "--noclear"
  ];
  # agetty's environment is what the login shell inherits — set TERM here
  # so readline/bash use modern escape sequences matching most host
  # terminals (kitty, iTerm, Terminal.app).
  systemd.services."getty@hvc0".environment.TERM = "xterm-256color";
  users.users.root.initialHashedPassword = lib.mkDefault "";
  users.users.root.shell = lib.mkDefault pkgs.bashInteractive;

  # ---- Networking ---------------------------------------------------------
  # nixvm wires virtio-net to vmnet (Apple's framework). Vmnet shared mode
  # runs a built-in DHCP server, so just enable the client.
  networking.useDHCP = lib.mkDefault true;
  networking.firewall.enable = lib.mkDefault false;

  # ---- Image build (systemd-repart, no runInLinuxVM) ---------------------
  image.repart.name = lib.mkDefault config.system.image.id;

  # ESP: vfat, holds the UKI at the EFI fallback path so libkrun's EDK2
  # picks it up directly with no bootloader install step.
  image.repart.partitions."10-esp".contents."/EFI/BOOT/BOOT${lib.toUpper pkgs.stdenv.hostPlatform.efiArch}.EFI".source =
    "${config.system.build.uki}/${config.system.boot.loader.ukiFile}";
  image.repart.partitions."10-esp".repartConfig.Type = "esp";
  image.repart.partitions."10-esp".repartConfig.Format = "vfat";
  # NixOS aarch64 UKIs hover around 70 MiB; 256M leaves headroom and
  # keeps the whole image inside the determinate-nixd builder's tmpfs.
  image.repart.partitions."10-esp".repartConfig.SizeMinBytes = "256M";
  image.repart.partitions."10-esp".repartConfig.SizeMaxBytes = "256M";

  # Root: small ext4 — the closure ships from the host via virtiofs, the
  # guest only needs space for /var, /etc, /tmp, /home runtime state.
  image.repart.partitions."20-root".repartConfig.Type = "root";
  image.repart.partitions."20-root".repartConfig.Format = "ext4";
  image.repart.partitions."20-root".repartConfig.Label = "nixos";
  image.repart.partitions."20-root".repartConfig.SizeMinBytes = "32M";
  image.repart.partitions."20-root".repartConfig.SizeMaxBytes = "32M";

  documentation.enable = lib.mkDefault false;
  documentation.man.enable = lib.mkDefault false;
}
