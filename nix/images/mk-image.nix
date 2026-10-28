# Shared base for the OCI images: passwd/group entries, CA certificates, a
# shell (initdb needs /bin/sh) and enough tools to
# `docker exec -it <container> bash` for debugging.
#
# `callPackage` this file to get a function `{ name, config }: <image>`.
{
  dockerTools,
  cacert,
  bashInteractive,
  coreutils-full,
  htop,
}:

{
  name,
  config,
}:
dockerTools.buildLayeredImage {
  inherit name;
  tag = "latest";
  contents = [
    # root and nobody, plus `user` (UID/GID 1000), which the images run as by
    # default.
    (dockerTools.fakeNss.override {
      extraPasswdLines = [ "user:x:1000:1000:user:/var/empty:/bin/sh" ];
      extraGroupLines = [ "user:x:1000:" ];
    })
    cacert
    bashInteractive
    coreutils-full
    htop
  ];
  extraCommands = ''
    mkdir -p -m 1777 tmp var/tmp
    mkdir -p -m 0755 run
  '';
  # Run as `user` unless an image says otherwise; override with e.g.
  # `docker run --user` to match the owner of a bind mount.
  config = {
    User = "1000:1000";
  }
  // config
  // {
    Env = [
      "SSL_CERT_FILE=${cacert}/etc/ssl/certs/ca-bundle.crt"
      "TMPDIR=/tmp"
    ]
    ++ config.Env or [ ];
  };
}
