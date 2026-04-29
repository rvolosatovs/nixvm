# Minimal test flake for nixvm.
#
# Run via nixvm:     nixvm run path:./examples/test-vm
{
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.11";
  inputs.nixvm.url = "git+file:../..";

  outputs =
    { nixpkgs, nixvm, ... }:
    {
      nixosConfigurations.default = nixpkgs.lib.nixosSystem {
        system = "aarch64-linux";
        modules = [
          nixvm.nixosModules.guest
          ./module.nix
        ];
      };
    };
}
