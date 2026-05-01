# Minimal smoke-test image. The host↔guest contract (single-partition
# image.repart layout, virtiofs+overlay /nix/store, autologin on hvc0,
# DHCP, closure registration via regInfo=) lives in
# nixvm.nixosModules.guest — this module just adds image identity and
# stateVersion.
{
  system.image.id = "minimal";
  system.image.version = "1";
  system.stateVersion = "25.11";
  networking.hostName = "minimal";
}
