# The whole Cargo workspace, built with crate2nix: every crate (third-party and
# workspace) is its own derivation, so a change only rebuilds the crates it
# touches and the crates that depend on them.
#
# Cargo.nix is generated from Cargo.lock by `nix run .#generate-cargo-nix`
# (which the pre-commit hook runs when a Cargo.toml or Cargo.lock changes).
{
  lib,
  pkgs,
  makeWrapper,
  runCommand,
  stdenvNoCC,
  symlinkJoin,
  cacert,
  cargo,
  cargo-nextest,
  jq,
  meilisearch,
  nix,
  nix-eval-jobs,
  postgresql,
}:

let
  cargoNix = import ./Cargo.nix { inherit pkgs; };
  # crate2nix resolves features separately for each workspace member, so
  # building each binary on its own would build a shared library once per
  # feature set it ends up with. Like cargo, unify features across the
  # workspace instead: resolve them once from a root that depends on every
  # member, and build every binary from that one graph.
  workspaceCrates =
    let
      inherit (cargoNix) internal;
      root = "nix-fetch-indexer (workspace)";
    in
    (internal.builtRustCratesWithFeatures {
      packageId = root;
      crateConfigs = internal.crates // {
        ${root} = {
          crateName = root;
          version = "0";
          dependencies = map (name: {
            inherit name;
            packageId = name;
          }) (lib.attrNames cargoNix.workspaceMembers);
        };
      };
      features = [ ];
      runTests = false;
      buildRustCrateForPkgsFunc = pkgs: pkgs.buildRustCrate;
    }).crates;
  crate = name: workspaceCrates.${name};

  # Each binary is a package of its own, wrapped with these on its PATH, so a
  # consumer (e.g. an image) only pulls in the binary it uses.
  bins =
    lib.mapAttrs
      (
        name: path:
        runCommand name
          {
            nativeBuildInputs = [ makeWrapper ];
            meta = meta // {
              mainProgram = name;
            };
          }
          (
            if path == [ ] then
              ''
                mkdir -p $out/bin
                ln -s ${crate name}/bin/${name} $out/bin/${name}
              ''
            else
              ''
                makeWrapper ${crate name}/bin/${name} $out/bin/${name} \
                  --suffix PATH : ${lib.makeBinPath path}
              ''
          )
      )
      {
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

  # Each workspace crate's tests (unit + integration), run separately with
  # nextest.
  #
  # crate2nix builds the crate under test in place of its normal build
  # throughout the dependency graph, so a dev-dependency that depends back on
  # it (nfi-service -> nfi-testenv -> nfi-service) or the crate itself (nfi-auth
  # enables test features that way) would be a cycle. Those edges point at the
  # crate's normal library (`libId`) instead. Like cargo, the integration tests
  # must link that same library, not a second copy, or its types differ from
  # the ones the dev-dependencies use. So they are built as a separate package
  # (`testsId`) that depends on the library, while the crate's own test build
  # only does the unit tests.
  testsFor =
    name:
    let
      inherit (cargoNix) internal;
      libId = "${name} (lib)";
      testsId = "${name} (integration tests)";
      self = internal.crates.${name};
      # All of the crate's features, so feature-gated tests run too (in the
      # workspace, other crates enable e.g. yoink-api's `client`).
      testFeatures = [
        "default"
      ]
      ++ lib.attrNames (self.features or { })
      ++ lib.concatMap (d: d.features or [ ]) (
        lib.filter (d: d.packageId == name) (self.devDependencies or [ ])
      );
      retarget = map (d: if d.packageId == name then d // { packageId = libId; } else d);
      without =
        paths: src:
        lib.cleanSourceWith {
          inherit src;
          filter = path: _: !lib.elem (lib.removePrefix "${toString self.src.origSrc}/" path) paths;
        };
      crateConfigs =
        lib.mapAttrs (_: c: c // { dependencies = retarget (c.dependencies or [ ]); }) internal.crates
        // {
          ${name} = self // {
            src = without [ "tests" ] self.src;
            devDependencies = lib.filter (d: d.packageId != name) (self.devDependencies or [ ]);
          };
          ${libId} = self // {
            devDependencies = [ ];
          };
          ${testsId} = self // {
            src = without [ "src/lib.rs" ] self.src;
            dependencies = self.dependencies or [ ] ++ [
              {
                inherit name;
                packageId = libId;
                features = testFeatures;
              }
            ];
            devDependencies = retarget (lib.filter (d: d.packageId != name) (self.devDependencies or [ ]));
          };
        };
      testBuild =
        packageId:
        (internal.builtRustCratesWithFeatures {
          inherit packageId crateConfigs;
          features = testFeatures;
          runTests = true;
          buildRustCrateForPkgsFunc = pkgs: pkgs.buildRustCrate;
        }).crates.${packageId}.override
          (_: {
            buildTests = true;
          });
    in
    nextest {
      inherit name;
      unitTests = testBuild name;
      integrationTests =
        if builtins.pathExists "${toString self.src.origSrc}/tests" then testBuild testsId else null;
    };

  # `cargo metadata` for the workspace's own packages, which nextest needs
  # alongside the test binaries. It is built from the Cargo.toml files alone,
  # with empty stand-ins for the targets cargo looks for, so a code change
  # doesn't change it and re-run every crate's tests.
  cargoMetadata =
    runCommand "nix-fetch-indexer-cargo-metadata.json"
      {
        nativeBuildInputs = [ cargo ];
        src = lib.fileset.toSource {
          root = ../.;
          fileset = lib.fileset.unions [
            ../Cargo.toml
            ../Cargo.lock
            (lib.fileset.fileFilter (f: f.name == "Cargo.toml") ../crates)
          ];
        };
      }
      ''
        cp -r --no-preserve=mode $src workspace
        for crate in workspace/crates/*/; do
          mkdir -p $crate/src
          touch $crate/src/lib.rs $crate/src/main.rs
        done
        CARGO_HOME=$TMPDIR cargo metadata --no-deps --offline --format-version 1 \
          --manifest-path workspace/Cargo.toml > $out
      '';

  # Runs a crate's test binaries (as built by buildRustCrate with buildTests)
  # with nextest, from the crate's directory as cargo would.
  nextest =
    {
      name,
      unitTests,
      integrationTests,
    }:
    stdenvNoCC.mkDerivation {
      name = "nextest-${name}";
      # Unpacked to the same directory the tests were compiled in, which
      # CARGO_MANIFEST_DIR points at.
      inherit (cargoNix.internal.crates.${name}) src;
      nativeBuildInputs = [
        cargo-nextest
        jq
        # The integration tests start ephemeral Postgres and Meilisearch
        # instances; NFI_TEST_REQUIRE_SERVICES makes them fail rather than
        # skip without them.
        postgresql
        meilisearch
        # reqwest's platform verifier refuses to build a client without CA
        # certificates, even for plain-HTTP test servers; cacert's setup hook
        # sets SSL_CERT_FILE inside the sandbox.
        cacert
      ];
      env.NFI_TEST_REQUIRE_SERVICES = "1";
      # Tests bind localhost ports.
      __darwinAllowLocalNetworking = true;
      dontConfigure = true;
      buildPhase = ''
        runHook preBuild

        # nextest runs each package's tests from its directory under the
        # workspace root; make that this crate's source.
        crate_dir=$(jq -r --arg name ${name} \
          '.packages[] | select(.name == $name) | .manifest_path | rtrimstr("/Cargo.toml")' \
          ${cargoMetadata})
        root=$(jq -r .workspace_root ${cargoMetadata})
        mkdir -p "$NIX_BUILD_TOP/workspace/$(dirname "''${crate_dir#"$root"/}")"
        ln -s "$PWD" "$NIX_BUILD_TOP/workspace/''${crate_dir#"$root"/}"
        cp ${../Cargo.toml} "$NIX_BUILD_TOP/workspace/Cargo.toml"

        # buildRustCrate puts a crate's test binaries in tests/: one per
        # tests/*.rs (or tests/*/main.rs) file, one per binary target, and one
        # for the library (named after it, plus a hash). Take the integration
        # tests from their own build and the rest from the unit-test build.
        package_id=$(jq -r --arg name ${name} '.packages[] | select(.name == $name) | .id' ${cargoMetadata})
        test -d ${unitTests}/tests ${
          lib.optionalString (integrationTests != null) "-a -d ${integrationTests}/tests"
        }
        for f in ${unitTests}/tests/* ${
          lib.optionalString (integrationTests != null) "${integrationTests}/tests/*"
        }; do
          b=$(basename "$f")
          if [ -e "tests/$b.rs" ] || [ -e "tests/$b/main.rs" ]; then
            [[ $f == ${toString integrationTests}/* ]] || continue
            kind=test id="${name}::$b"
          elif [[ $f != ${unitTests}/* ]]; then
            continue
          elif [[ $b == ${lib.replaceStrings [ "-" ] [ "_" ] name}-* ]]; then
            kind=lib id="${name}"
          else
            kind=bin id="${name}::bin/$b"
          fi
          jq -n --arg id "$id" --arg name "$b" --arg kind "$kind" --arg pkg "$package_id" --arg path "$f" \
            '{($id): {"binary-id": $id, "binary-name": $name, "package-id": $pkg, "kind": $kind, "binary-path": $path, "build-platform": "target"}}'
        done | jq -s \
          '{"rust-build-meta": {"target-directory": "/nonexistent", "base-output-directories": ["debug"], "non-test-binaries": {}, "build-script-out-dirs": {}, "linked-paths": [], "target-platforms": [{"triple": "${stdenvNoCC.hostPlatform.rust.rustcTarget}", "target-features": "unknown"}], "target-platform": null}, "rust-binaries": (add // {})}' \
          > $NIX_BUILD_TOP/binaries-metadata.json

        cargo-nextest nextest run \
          --cargo-metadata ${cargoMetadata} \
          --binaries-metadata $NIX_BUILD_TOP/binaries-metadata.json \
          --workspace-remap $NIX_BUILD_TOP/workspace \
          --no-fail-fast --no-tests=pass --show-progress=none --color=never

        runHook postBuild
      '';
      installPhase = "touch $out";
    };

  tests = lib.mapAttrs (name: _: testsFor name) cargoNix.workspaceMembers;

  meta = {
    description = "Index NixOS binary cache closures and query minimal fetch sets";
    license = lib.licenses.mit;
  };
in
symlinkJoin {
  name = "nix-fetch-indexer-${(lib.importTOML ../Cargo.toml).workspace.package.version}";
  paths = lib.attrValues bins;
  passthru = bins // {
    inherit cargoNix cargoMetadata tests;
  };
  meta = meta // {
    mainProgram = "nfi";
  };
}
