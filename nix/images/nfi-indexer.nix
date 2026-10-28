# Swap the binary with e.g. `nfi-indexer-image.override { nfi-indexer = ...; }`;
# any package with `bin/nfi-indexer` works.
{
  lib,
  mkImage,
  nfi-indexer,
}:

mkImage {
  name = "nfi-indexer";
  config = {
    Entrypoint = [ (lib.getExe' nfi-indexer "nfi-indexer") ];
    WorkingDir = "/tmp";
  };
}
