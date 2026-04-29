# Minimal test flake for nixvm.
#
# Run via nixvm:     nixvm run ./examples/minimal
{
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.11";
  inputs.nixvm.url = "github:rvolosatovs/nixvm";

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
