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
              # reqwest's platform verifier refuses to build a client without
              # CA certificates, even for plain-HTTP test servers; cacert's
              # setup hook sets SSL_CERT_FILE inside the sandbox.
              pkgs.cacert
            ];
            env.NFI_TEST_REQUIRE_SERVICES = "1";
            # Tests bind localhost ports.
            __darwinAllowLocalNetworking = true;

            postInstall = ''
              wrapProgram $out/bin/nfi-indexer --suffix PATH : ${lib.makeBinPath [ pkgs.nix-eval-jobs ]}
              wrapProgram $out/bin/nfi --suffix PATH : ${lib.makeBinPath [ pkgs.nix ]}
              wrapProgram $out/bin/yoink --suffix PATH : ${lib.makeBinPath [ pkgs.nix ]}
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
          # Shared base for the OCI images: passwd/group entries, CA
          # certificates, a shell (initdb needs /bin/sh) and enough tools to
          # `docker exec -it <container> bash` for debugging.
          image =
            {
              name,
              config,
            }:
            pkgs.dockerTools.buildLayeredImage {
              inherit name;
              tag = "latest";
              contents = [
                pkgs.dockerTools.fakeNss
                pkgs.cacert
                pkgs.bashInteractive
                pkgs.coreutils-full
                pkgs.htop
              ];
              extraCommands = ''
                mkdir -p -m 1777 tmp var/tmp
                mkdir -p -m 0755 run
              '';
              config = config // {
                Env = [
                  "SSL_CERT_FILE=${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt"
                  "TMPDIR=/tmp"
                ]
                ++ config.Env or [ ];
              };
            };
        in
        {
          default = workspace;
          nix-fetch-indexer = workspace;
          nfi = single "nfi";
          nfi-admin = single "nfi-admin";
          nfi-indexer = single "nfi-indexer";
          nfi-service = single "nfi-service";
          yoink = single "yoink";
          yoink-service = single "yoink-service";
        }
        // linuxOnly pkgs {
          # OCI images (load with `docker load < result`).
          nfi-service-image = image {
            name = "nfi-service";
            config = {
              Entrypoint = [ "${workspace}/bin/nfi-service" ];
              Env = [
                "NFI_LISTEN=0.0.0.0:8080"
                # The admin listener stays inside the container by default.
                # To reach it from outside, override NFI_ADMIN_LISTEN, set
                # NFI_ADMIN_TOKEN_FILE (or NFI_ADMIN_OIDC) and publish the
                # port on a private address.
                "NFI_ADMIN_LISTEN=127.0.0.1:8081"
              ];
              ExposedPorts = {
                "8080/tcp" = { };
              };
              User = "65534:65534";
            };
          };
          yoink-service-image = image {
            name = "yoink-service";
            config = {
              Entrypoint = [ "${workspace}/bin/yoink-service" ];
              Env = [ "YOINK_LISTEN=0.0.0.0:8090" ];
              ExposedPorts = {
                "8090/tcp" = { };
              };
              User = "65534:65534";
            };
          };
          nfi-indexer-image = image {
            name = "nfi-indexer";
            config = {
              Entrypoint = [ "${workspace}/bin/nfi-indexer" ];
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
