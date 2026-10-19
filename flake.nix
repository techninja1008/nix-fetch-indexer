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
          inherit (pkgs) lib;
          workspace = pkgs.rustPlatform.buildRustPackage {
            pname = "nix-fetch-indexer";
            version = (lib.importTOML ./Cargo.toml).workspace.package.version;
            src = lib.fileset.toSource {
              root = ./.;
              fileset = lib.fileset.unions [
                ./Cargo.toml
                ./Cargo.lock
                ./LICENSE
                ./crates
              ];
            };
            cargoLock.lockFile = ./Cargo.lock;
            nativeBuildInputs = [ pkgs.makeWrapper ];

            # The integration tests start ephemeral Postgres and Meilisearch
            # instances; require them so the tests cannot silently skip.
            nativeCheckInputs = [
              pkgs.postgresql
              pkgs.meilisearch
            ];
            env.NFI_TEST_REQUIRE_SERVICES = "1";
            # Tests bind localhost ports.
            __darwinAllowLocalNetworking = true;

            postInstall = ''
              wrapProgram $out/bin/nfi-indexer --suffix PATH : ${lib.makeBinPath [ pkgs.nix-eval-jobs ]}
              wrapProgram $out/bin/nfi --suffix PATH : ${lib.makeBinPath [ pkgs.nix ]}
              # For --embedded-postgres / --embedded-meilisearch (single-node mode).
              wrapProgram $out/bin/nfi-service --suffix PATH : ${
                lib.makeBinPath [
                  pkgs.postgresql
                  pkgs.meilisearch
                ]
              }
              rm -f $out/bin/nfi-testenv
            '';

            meta = {
              description = "Index NixOS binary cache closures and query minimal fetch sets";
              license = lib.licenses.mit;
              mainProgram = "nfi";
            };
          };
          single =
            name:
            pkgs.runCommand name { meta.mainProgram = name; } ''
              mkdir -p $out/bin
              ln -s ${workspace}/bin/${name} $out/bin/${name}
              if [ -e ${workspace}/bin/.${name}-wrapped ]; then
                ln -s ${workspace}/bin/.${name}-wrapped $out/bin/.${name}-wrapped
              fi
            '';
        in
        {
          default = workspace;
          nix-fetch-indexer = workspace;
          nfi = single "nfi";
          nfi-admin = single "nfi-admin";
          nfi-indexer = single "nfi-indexer";
          nfi-service = single "nfi-service";
        }
        // linuxOnly pkgs {
          # OCI images (load with `docker load < result`).
          nfi-service-image = pkgs.dockerTools.buildLayeredImage {
            name = "nfi-service";
            tag = "latest";
            contents = [ pkgs.cacert ];
            config = {
              Entrypoint = [ "${workspace}/bin/nfi-service" ];
              Env = [
                "NFI_LISTEN=0.0.0.0:8080"
                # The admin listener stays inside the container by default.
                # To reach it from outside, override NFI_ADMIN_LISTEN, set
                # NFI_ADMIN_TOKEN and publish the port on a private address.
                "NFI_ADMIN_LISTEN=127.0.0.1:8081"
                "SSL_CERT_FILE=${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt"
              ];
              ExposedPorts = {
                "8080/tcp" = { };
              };
              User = "65534:65534";
            };
          };
          nfi-indexer-image = pkgs.dockerTools.buildLayeredImage {
            name = "nfi-indexer";
            tag = "latest";
            contents = [
              pkgs.cacert
              pkgs.dockerTools.fakeNss
            ];
            extraCommands = "mkdir -m 1777 tmp";
            config = {
              Entrypoint = [ "${workspace}/bin/nfi-indexer" ];
              Env = [
                "SSL_CERT_FILE=${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt"
                "TMPDIR=/tmp"
              ];
              WorkingDir = "/tmp";
            };
          };
        }
      );

      checks = forAll (
        pkgs:
        {
          # Builds everything and runs the full test suite (unit + integration).
          build = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
        }
      );

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
