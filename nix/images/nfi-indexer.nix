# Swap the binary with e.g. `nfi-indexer-image.override { nfi-indexer = ...; }`;
# any package whose `meta.mainProgram` is set works.
{
  lib,
  mkImage,
  nfi-indexer,
}:

mkImage {
  name = "nfi-indexer";
  config = {
    Entrypoint = [ (lib.getExe nfi-indexer) ];
    WorkingDir = "/tmp";
  };
}
