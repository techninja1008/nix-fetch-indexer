{
  description = "Index NixOS binary cache closures and query minimal fetch sets";

  # Channel tarball (as used by the default `nixpkgs` registry entry).
  inputs.nixpkgs.url = "https://channels.nixos.org/nixos-unstable/nixexprs.tar.xz";

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];
      forAll = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
      linuxOnly = pkgs: pkgs.lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux;
    in
    {
      packages = forAll (
        pkgs:
        let
          workspace = pkgs.callPackage ./nix/package.nix { };
          bins = {
            inherit (workspace) nfi yoink;
            nfi-admin = workspace.nfi_admin;
            nfi-indexer = workspace.nfi_indexer;
            nfi-service = workspace.nfi_service;
            yoink-service = workspace.yoink_service;
          };
          mkImage = pkgs.callPackage ./nix/images/mk-image.nix { };
          # Each image takes its binary as an argument, so e.g.
          # `nfi-service-image.override { nfi-service = ...; }` swaps it.
          image =
            name:
            pkgs.callPackage ./nix/images/${name}.nix {
              inherit mkImage;
              ${name} = bins.${name};
            };
        in
        {
          default = workspace;
          nix-fetch-indexer = workspace;
        }
        // bins
        // linuxOnly pkgs {
          # OCI images (load with `docker load < result`).
          nfi-service-image = image "nfi-service";
          yoink-service-image = image "yoink-service";
          nfi-indexer-image = image "nfi-indexer";
        }
      );

      checks = forAll (pkgs: {
        # Builds everything and runs the full test suite (unit + integration).
        build = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      });

      devShells = forAll (pkgs: {
        default = pkgs.mkShell {
          packages = [
            pkgs.cargo
            pkgs.rustc
            pkgs.clippy
            pkgs.rustfmt
            pkgs.rust-analyzer
            pkgs.postgresql
            pkgs.meilisearch
            pkgs.nix-eval-jobs
          ];
        };
      });

      formatter = forAll (pkgs: pkgs.nixfmt-rfc-style);
    };
}
