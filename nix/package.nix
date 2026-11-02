# The whole Cargo workspace, built with crane: third-party dependencies are
# compiled once in their own derivation, which is reused until Cargo.toml or
# Cargo.lock change, and the test suite runs separately (`passthru.tests`).
{
  lib,
  craneLib,
  makeWrapper,
  cacert,
  meilisearch,
  nix,
  nix-eval-jobs,
  postgresql,
}:

let
  # Each binary gets its own output, wrapped with these on its PATH, so a
  # consumer (e.g. an image) only pulls in the binary it uses. `out` links
  # to all of them.
  bins = {
    nfi = [ nix ];
    nfi-admin = [ ];
    nfi-indexer = [ nix-eval-jobs ];
    # For --embedded-postgres / --embedded-meilisearch (single-node mode).
    nfi-service = [
      postgresql
      meilisearch
    ];
    yoink = [ nix ];
    yoink-service = [ ];
  };
  # Output names become shell variables, so they can't contain `-`.
  outputName = lib.replaceStrings [ "-" ] [ "_" ];

  commonArgs = {
    pname = "nix-fetch-indexer";
    version = (lib.importTOML ../Cargo.toml).workspace.package.version;
    src = lib.fileset.toSource {
      root = ../.;
      fileset = lib.fileset.unions [
        ../Cargo.toml
        ../Cargo.lock
        ../LICENSE
        ../crates
      ];
    };
    strictDeps = true;
  };

  # Only the dependencies (including dev-dependencies, for the tests), built
  # from a copy of the source with every crate's code stubbed out.
  cargoArtifacts = craneLib.buildDepsOnly commonArgs;

  workspace = craneLib.buildPackage (
    commonArgs
    // {
      inherit cargoArtifacts;
      nativeBuildInputs = [ makeWrapper ];
      # See `passthru.tests`.
      doCheck = false;

      outputs = [ "out" ] ++ map outputName (lib.attrNames bins);

      postInstall = ''
        rm -f $out/bin/nfi-testenv
      ''
      + lib.concatStrings (
        lib.mapAttrsToList (
          name: path:
          let
            output = "$" + outputName name;
            dir = "${output}/bin";
          in
          # crane's hooks that keep the vendored sources and the toolchain out
          # of the runtime closure only process `$out`, so run them here.
          ''
            moveToOutput bin/${name} "${output}"
            removeReferencesToVendoredSources "${output}"
            removeReferencesToRustToolchain "${output}"
          ''
          + lib.optionalString (path != [ ]) ''
            wrapProgram ${dir}/${name} --suffix PATH : ${lib.makeBinPath path}
          ''
          + ''
            ln -s ${dir}/${name} $out/bin/${name}
          ''
        ) bins
      );

      # Outputs share `meta`, so replace each binary's output with one whose
      # `mainProgram` is that binary (for `nix run` and `lib.getExe`). `all`
      # holds the outputs as built, before passthru is applied.
      passthru =
        lib.listToAttrs (
          map (
            name:
            lib.nameValuePair (outputName name) (
              lib.findFirst (o: o.outputName == outputName name) null workspace.all
              // {
                meta = workspace.meta // {
                  mainProgram = name;
                };
              }
            )
          ) (lib.attrNames bins)
        )
        // {
          inherit cargoArtifacts;
          # The full test suite (unit + integration).
          tests = craneLib.cargoTest (
            commonArgs
            // {
              inherit cargoArtifacts;
              # The integration tests start ephemeral Postgres and Meilisearch
              # instances; require them so the tests cannot silently skip.
              nativeBuildInputs = [
                postgresql
                meilisearch
                # reqwest's platform verifier refuses to build a client
                # without CA certificates, even for plain-HTTP test servers;
                # cacert's setup hook sets SSL_CERT_FILE inside the sandbox.
                cacert
              ];
              env.NFI_TEST_REQUIRE_SERVICES = "1";
              # Tests bind localhost ports.
              __darwinAllowLocalNetworking = true;
              # Nothing to keep but the fact that they passed.
              doInstallCargoArtifacts = false;
            }
          );
        };

      meta = {
        description = "Index NixOS binary cache closures and query minimal fetch sets";
        license = lib.licenses.mit;
        mainProgram = "nfi";
      };
    }
  );
in
workspace
