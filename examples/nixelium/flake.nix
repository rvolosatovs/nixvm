# nixvm example: import the nixelium flake and produce a libkrun-bootable
# raw EFI image of its baseline NixOS module.
#
# Build the image:   nix build path:./examples/nixelium
# Run via nixvm:     nixvm path:./examples/nixelium#default
{
  description = "nixvm image built from nixelium's nixosModules.default";

  inputs.nixelium.url = "github:rvolosatovs/nixelium";

  outputs =
    { self, nixelium }:
    let
      system = "aarch64-linux";
      nixpkgs = nixelium.inputs.nixpkgs-nixos;
      pkgs = nixpkgs.legacyPackages.${system};

      nixos = nixpkgs.lib.nixosSystem {
        inherit system;
        modules = [
          nixelium.nixosModules.default
          ./module.nix
        ];
      };
      imageFile = pkgs.runCommand "${nixos.config.image.repart.name}.raw" { } ''
        ln -s ${nixos.config.system.build.image}/${nixos.config.image.fileName} $out
      '';
    in
    {
      packages.${system}.default = imageFile;
      packages.aarch64-darwin.default = self.packages.${system}.default;
    };
}
