# Adapter module: makes nixelium's baseline NixOS module bootable under libkrun.
#
# Overrides nixelium defaults that don't fit a headless ephemeral VM:
#   - lanzaboote / systemd-boot off → UKI dropped at the EFI fallback path
#   - EFI variables off (libkrun's EDK2 doesn't expose persistent NVRAM)
#   - tailscale / firewall off (no out-of-band auth, ephemeral runs)
#   - home-manager users cleared (nixelium's user closure is huge — rust
#     toolchains, neovim, fonts — and not needed for a smoke-test VM)
#
# Adds the libkrun contract: console=hvc0, virtio modules in initrd,
# /nix/store from virtiofs, autologin on hvc0, image.repart layout.
{
  config,
  lib,
  modulesPath,
  pkgs,
  ...
}:

let
  imageId = "nixvm-nixelium";
  imageVersion = "1";
in
{
  imports = [ "${modulesPath}/image/repart.nix" ];

  system.image.id = imageId;
  system.image.version = imageVersion;

  # ---- Boot ---------------------------------------------------------------
  boot.lanzaboote.enable = false;
  boot.loader.systemd-boot.enable = lib.mkForce false;
  boot.loader.efi.canTouchEfiVariables = lib.mkForce false;

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

  fileSystems."/nix/store" = {
    device = "nixstore";
    fsType = "virtiofs";
    options = [
      "ro"
      "nofail"
    ];
    neededForBoot = true;
  };

  # ---- Login --------------------------------------------------------------
  services.getty.autologinUser = "root";
  services.getty.extraArgs = [
    "--keep-baud"
    "--noclear"
  ];
  systemd.services."getty@hvc0".environment.TERM = "xterm-256color";

  # ---- Disable nixelium bits that don't fit an ephemeral headless VM ------
  services.tailscale.enable = lib.mkForce false;
  networking.firewall.enable = lib.mkForce false;

  home-manager.users = lib.mkForce { root.home.stateVersion = "25.11"; };

  networking.hostName = "nixelium-vm";

  # ---- Image build (systemd-repart) --------------------------------------
  image.repart.name = imageId;

  image.repart.partitions."10-esp".contents."/EFI/BOOT/BOOT${lib.toUpper pkgs.stdenv.hostPlatform.efiArch}.EFI".source =
    "${config.system.build.uki}/${config.system.boot.loader.ukiFile}";
  image.repart.partitions."10-esp".repartConfig.Type = "esp";
  image.repart.partitions."10-esp".repartConfig.Format = "vfat";
  image.repart.partitions."10-esp".repartConfig.SizeMinBytes = "256M";
  image.repart.partitions."10-esp".repartConfig.SizeMaxBytes = "256M";

  image.repart.partitions."20-root".repartConfig.Type = "root";
  image.repart.partitions."20-root".repartConfig.Format = "ext4";
  image.repart.partitions."20-root".repartConfig.Label = "nixos";
  image.repart.partitions."20-root".repartConfig.SizeMinBytes = "32M";
  image.repart.partitions."20-root".repartConfig.SizeMaxBytes = "32M";

  documentation.enable = lib.mkDefault false;
  documentation.man.enable = lib.mkDefault false;
}
