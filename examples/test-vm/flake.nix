# Minimal test flake for nixvm.
#
# Build the image:   nix build path:./examples/test-vm
# Run via nixvm:     nixvm path:./examples/test-vm#default
{
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.11";

  outputs =
    { self, nixpkgs }:
    let
      system = "aarch64-linux";
      pkgs = nixpkgs.legacyPackages.${system};

      nixos = nixpkgs.lib.nixosSystem {
        inherit system;
        modules = [ ./module.nix ];
      };
      # The image.repart module exposes the built disk image as
      # `system.build.image`. The actual file inside is named after the
      # image's id/version with .raw extension; `image.repart.imageFile`
      # gives the basename. We expose both: the wrapping derivation (a
      # directory) and a symlink to the file directly.
      image = nixos.config.system.build.image;
      imageFile = pkgs.runCommand "${nixos.config.image.repart.name}.raw" { } ''
        ln -s ${nixos.config.system.build.image}/${nixos.config.image.fileName} $out
      '';
    in
    {
      packages.${system}.default = imageFile;

      # Convenience for cross-evaluation from aarch64-darwin: same attribute
      # under the host system, since the image-building derivation is what
      # nixvm wants and it's a Linux derivation that nix-daemon will build.
      packages.aarch64-darwin.default = self.packages.${system}.default;
    };
}
