{
  description = "Index NixOS binary cache closures and query minimal fetch sets";

  # Channel tarball (as used by the default `nixpkgs` registry entry).
  inputs.nixpkgs.url = "https://channels.nixos.org/nixos-unstable/nixexprs.tar.xz";
  inputs.git-hooks = {
    url = "github:cachix/git-hooks.nix";
    inputs.nixpkgs.follows = "nixpkgs";
    inputs.flake-compat.follows = "";
  };

  outputs =
    {
      self,
      nixpkgs,
      git-hooks,
    }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAll = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
      # Git hooks, installed by `nix develop` and run on every file in CI
      # (.github/workflows/checks.yml).
      preCommit =
        pkgs:
        let
          inherit (pkgs.stdenv.hostPlatform) system;
        in
        git-hooks.lib.${system}.run {
          src = ./.;
          hooks = {
            # Needs network access (cargo fetches crate manifests), so this
            # isn't a flake check.
            cargo-nix = {
              enable = true;
              name = "nix/Cargo.nix is up to date";
              entry = pkgs.lib.getExe self.packages.${system}.generate-cargo-nix;
              files = "(^|/)Cargo\\.(toml|lock)$";
              pass_filenames = false;
            };
            nixfmt = {
              enable = true;
              excludes = [ "^nix/Cargo\\.nix$" ];
            };
            rustfmt = {
              enable = true;
              packageOverrides = { inherit (pkgs) cargo rustfmt; };
            };
          };
        };
    in
    {
      packages = forAll (
        pkgs:
        let
          workspace = pkgs.callPackage ./nix/package.nix { };
          bins = {
            inherit (workspace)
              nfi
              nfi-admin
              nfi-indexer
              nfi-service
              yoink
              yoink-service
              ;
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
          generate-cargo-nix = pkgs.callPackage ./nix/generate-cargo-nix.nix { };
          # OCI images (load with `docker load < result`).
          nfi-service-image = image "nfi-service";
          yoink-service-image = image "yoink-service";
          nfi-indexer-image = image "nfi-indexer";
        }
        // bins
      );

      checks = forAll (
        pkgs:
        let
          workspace = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
        in
        {
          build = workspace;
        }
        # Each crate's tests (unit + integration), e.g. `test-nfi-service`.
        // pkgs.lib.mapAttrs' (name: pkgs.lib.nameValuePair "test-${name}") workspace.tests
      );

      devShells = forAll (
        pkgs:
        let
          hooks = preCommit pkgs;
        in
        {
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
            ]
            ++ hooks.enabledPackages;
            inherit (hooks) shellHook;
          };
        }
      );

      formatter = forAll (pkgs: pkgs.nixfmt-rfc-style);
    };
}
