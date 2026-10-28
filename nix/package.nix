# The whole Cargo workspace: every binary, built and tested together.
{
  lib,
  rustPlatform,
  makeWrapper,
  cacert,
  meilisearch,
  nix,
  nix-eval-jobs,
  postgresql,
}:

rustPlatform.buildRustPackage {
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
  cargoLock.lockFile = ../Cargo.lock;
  nativeBuildInputs = [ makeWrapper ];

  # The integration tests start ephemeral Postgres and Meilisearch
  # instances; require them so the tests cannot silently skip.
  nativeCheckInputs = [
    postgresql
    meilisearch
    # reqwest's platform verifier refuses to build a client without
    # CA certificates, even for plain-HTTP test servers; cacert's
    # setup hook sets SSL_CERT_FILE inside the sandbox.
    cacert
  ];
  env.NFI_TEST_REQUIRE_SERVICES = "1";
  # Tests bind localhost ports.
  __darwinAllowLocalNetworking = true;

  postInstall = ''
    wrapProgram $out/bin/nfi-indexer --suffix PATH : ${lib.makeBinPath [ nix-eval-jobs ]}
    wrapProgram $out/bin/nfi --suffix PATH : ${lib.makeBinPath [ nix ]}
    wrapProgram $out/bin/yoink --suffix PATH : ${lib.makeBinPath [ nix ]}
    # For --embedded-postgres / --embedded-meilisearch (single-node mode).
    wrapProgram $out/bin/nfi-service --suffix PATH : ${
      lib.makeBinPath [
        postgresql
        meilisearch
      ]
    }
    rm -f $out/bin/nfi-testenv
  '';

  meta = {
    description = "Index NixOS binary cache closures and query minimal fetch sets";
    license = lib.licenses.mit;
    mainProgram = "nfi";
  };
}
