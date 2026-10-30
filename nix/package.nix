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
in
rustPlatform.buildRustPackage (finalAttrs: {
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
      ''
        moveToOutput bin/${name} "${output}"
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
  # `mainProgram` is that binary (for `nix run` and `lib.getExe`). `all` holds
  # the outputs as built, before passthru is applied.
  passthru = lib.listToAttrs (
    map (
      name:
      lib.nameValuePair (outputName name) (
        lib.findFirst (o: o.outputName == outputName name) null finalAttrs.finalPackage.all
        // {
          meta = finalAttrs.finalPackage.meta // {
            mainProgram = name;
          };
        }
      )
    ) (lib.attrNames bins)
  );

  meta = {
    description = "Index NixOS binary cache closures and query minimal fetch sets";
    license = lib.licenses.mit;
    mainProgram = "nfi";
  };
})
