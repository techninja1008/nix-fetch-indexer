# Swap the binary with e.g.
# `yoink-service-image.override { yoink-service = ...; }`; any package with
# `bin/yoink-service` works.
{
  lib,
  mkImage,
  yoink-service,
}:

mkImage {
  name = "yoink-service";
  config = {
    Entrypoint = [ (lib.getExe' yoink-service "yoink-service") ];
    Env = [ "YOINK_LISTEN=0.0.0.0:8090" ];
    ExposedPorts = {
      "8090/tcp" = { };
    };
  };
}
