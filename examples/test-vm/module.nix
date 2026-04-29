# Minimal smoke-test image. The host↔guest contract (image.repart layout,
# UKI at the EFI fallback path, virtiofs+overlay /nix/store, autologin on
# hvc0, DHCP) lives in nixvm.nixosModules.guest — this module just adds
# image identity and stateVersion.
{
  system.image.id = "nixvm-test";
  system.image.version = "1";
  system.stateVersion = "25.11";
  networking.hostName = "nixvm";
}
