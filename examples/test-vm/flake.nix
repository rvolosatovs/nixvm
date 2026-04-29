# Minimal test flake for nixvm.
#
# Run via nixvm:     nixvm run path:./examples/test-vm
{
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.11";

  outputs =
    { nixpkgs, ... }:
    {
      nixosConfigurations.default = nixpkgs.lib.nixosSystem {
        system = "aarch64-linux";
        modules = [ ./module.nix ];
      };
    };
}
