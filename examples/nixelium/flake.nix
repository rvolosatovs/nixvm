# nixvm example: import the nixelium flake and produce a libkrun-bootable
# raw EFI image of its baseline NixOS module.
#
# Run via nixvm:     nixvm run ./examples/nixelium
{
  description = "nixvm image built from nixelium's nixosModules.default";

  inputs.nixelium.url = "github:rvolosatovs/nixelium";
  inputs.nixvm.url = "github:rvolosatovs/nixvm";

  outputs =
    { nixelium, nixvm, ... }:
    {
      nixosConfigurations.default = nixelium.inputs.nixpkgs-nixos.lib.nixosSystem {
        system = "aarch64-linux";
        modules = [
          nixvm.nixosModules.guest
          nixelium.nixosModules.default
          ./module.nix
        ];
      };
    };
}
