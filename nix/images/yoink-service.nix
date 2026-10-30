# Swap the binary with e.g.
# `yoink-service-image.override { yoink-service = ...; }`; any package whose
# `meta.mainProgram` is set works.
{
  lib,
  mkImage,
  yoink-service,
}:

mkImage {
  name = "yoink-service";
  config = {
    Entrypoint = [ (lib.getExe yoink-service) ];
    Env = [ "YOINK_LISTEN=0.0.0.0:8090" ];
    ExposedPorts = {
      "8090/tcp" = { };
    };
  };
}
