# Swap the binary with e.g. `nfi-service-image.override { nfi-service = ...; }`;
# any package whose `meta.mainProgram` is set works.
{
  lib,
  mkImage,
  nfi-service,
}:

mkImage {
  name = "nfi-service";
  config = {
    Entrypoint = [ (lib.getExe nfi-service) ];
    Env = [
      "NFI_LISTEN=0.0.0.0:8080"
      # The admin listener stays inside the container by default. To reach it
      # from outside, override NFI_ADMIN_LISTEN, set NFI_ADMIN_TOKEN_FILE (or
      # NFI_ADMIN_OIDC) and publish the port on a private address.
      "NFI_ADMIN_LISTEN=127.0.0.1:8081"
    ];
    ExposedPorts = {
      "8080/tcp" = { };
    };
  };
}
