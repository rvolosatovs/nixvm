# Adapter module: makes nixelium's baseline NixOS module bootable under
# nixvm. The host↔guest contract lives in nixvm.nixosModules.guest; this
# module only overrides nixelium defaults that don't fit a headless
# ephemeral VM.
#
# Overrides:
#   - lanzaboote / systemd-boot off (UKI dropped at the EFI fallback path)
#   - tailscale off (no out-of-band auth in ephemeral runs)
#   - home-manager users cleared (nixelium's user closure is huge — rust
#     toolchains, neovim, fonts — and not needed for a smoke-test VM)
{ lib, ... }:

{
  system.image.id = "nixvm-nixelium";
  system.image.version = "1";
  networking.hostName = "nixelium-vm";

  # nixelium enables these at normal priority — force them off here.
  boot.lanzaboote.enable = false;
  boot.loader.systemd-boot.enable = lib.mkForce false;
  services.tailscale.enable = lib.mkForce false;

  home-manager.users = lib.mkForce { root.home.stateVersion = "25.11"; };
}
