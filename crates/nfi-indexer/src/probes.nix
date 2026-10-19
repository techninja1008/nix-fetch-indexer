# Helper probes: calls of nixpkgs helpers whose build inputs are worth
# indexing because they are not in any runtime closure (e.g. fetchurl's
# bootstrap curl). See probes.rs for how the results are used.
#
# Each probe is one of:
#   { kind = "fetch"; a = <drv>; b = <drv>; }
#     The same helper called twice with every argument it is given changed
#     (URL, revision, hash, name, source). Only derivations common to both
#     calls are kept, so anything built from the arguments drops out.
#   { kind = "target"; targets = [ <drv> ... ]; }
#     Derivations that are themselves roots (e.g. stdenv).
#
# Probes guard on the helpers and arguments a nixpkgs version has, so the
# same file works from 24.05 to unstable; a probe that still fails to
# evaluate is skipped and recorded by the indexer.
{
  # The nixpkgs source tree (an unpacked nixexprs tarball).
  nixpkgs,
  system,
  # Directory with `a` and `b` subdirectories of dummy source files.
  srcs,
}:
let
  pkgs = import nixpkgs {
    localSystem = { inherit system; };
    config = { };
    overlays = [ ];
  };
  inherit (pkgs) lib;

  has = f: arg: (lib.functionArgs f) ? ${arg};
  # Valid SRI sha256 hashes that differ between the two calls.
  hash = {
    a = "sha256-" + lib.strings.replicate 43 "E" + "=";
    b = "sha256-" + lib.strings.replicate 43 "I" + "=";
  };
  # Older helpers only take `sha256`; an SRI value works for both.
  hashArg = f: x: if has f "hash" then { hash = hash.${x}; } else { sha256 = hash.${x}; };
  rev = {
    a = lib.strings.replicate 40 "a";
    b = lib.strings.replicate 40 "b";
  };
  src = x: srcs + "/${x}";

  fetch = f: mk: {
    kind = "fetch";
    a = f (mk "a");
    b = f (mk "b");
  };
  # A fetch probe for helper `f` with extra arguments, if `f` takes them.
  fetchWith =
    f: extra: mk:
    if lib.all (has f) (builtins.attrNames extra) then fetch f (x: mk x // extra) else null;
  url = x: ext: "https://nfi-probe-${x}.invalid/nfi-probe-${x}${ext}";

  gitArgs =
    f: x:
    {
      url = url x ".git";
      rev = rev.${x};
    }
    // hashArg f x;
  githubArgs =
    f: x:
    {
      owner = "nfi-probe-${x}";
      repo = "nfi-probe-${x}";
      rev = rev.${x};
    }
    // hashArg f x;

  probes = {
    fetchurl = fetch pkgs.fetchurl (x: { url = url x ".tar.gz"; } // hashArg pkgs.fetchurl x);
    fetchzip = fetch pkgs.fetchzip (x: { url = url x ".zip"; } // hashArg pkgs.fetchzip x);
    fetchpatch = fetch pkgs.fetchpatch (x: { url = url x ".patch"; } // hashArg pkgs.fetchpatch x);

    fetchgit = fetch pkgs.fetchgit (gitArgs pkgs.fetchgit);
    fetchgit-submodules = fetchWith pkgs.fetchgit { fetchSubmodules = true; } (gitArgs pkgs.fetchgit);
    fetchgit-leave-dot-git = fetchWith pkgs.fetchgit { leaveDotGit = true; } (gitArgs pkgs.fetchgit);
    fetchgit-lfs = fetchWith pkgs.fetchgit { fetchLFS = true; } (gitArgs pkgs.fetchgit);
    fetchgit-sparse = fetchWith pkgs.fetchgit { sparseCheckout = [ "nfi-probe" ]; } (gitArgs pkgs.fetchgit);
    fetchFromGitHub = fetch pkgs.fetchFromGitHub (githubArgs pkgs.fetchFromGitHub);
    fetchFromGitHub-submodules = fetchWith pkgs.fetchFromGitHub { fetchSubmodules = true; } (
      githubArgs pkgs.fetchFromGitHub
    );
    fetchFromGitLab = fetch pkgs.fetchFromGitLab (githubArgs pkgs.fetchFromGitLab);

    fetchsvn = fetch pkgs.fetchsvn (
      x:
      {
        url = url x "/svn";
        rev = if x == "a" then 1 else 2;
      }
      // hashArg pkgs.fetchsvn x
    );
    fetchhg = fetch pkgs.fetchhg (
      x:
      {
        url = url x "/hg";
        rev = rev.${x};
      }
      // hashArg pkgs.fetchhg x
    );
    fetchbzr = fetch pkgs.fetchbzr (
      x:
      {
        url = url x "/bzr";
        rev = if x == "a" then "1" else "2";
      }
      // hashArg pkgs.fetchbzr x
    );
    fetchcvs = fetch pkgs.fetchcvs (
      x:
      {
        cvsRoot = ":pserver:anonymous@nfi-probe-${x}.invalid:/cvs";
        module = "nfi-probe-${x}";
        date = if x == "a" then "2000-01-01" else "2000-01-02";
      }
      // hashArg pkgs.fetchcvs x
    );

    fetchCargoVendor =
      if pkgs.rustPlatform ? fetchCargoVendor then
        fetch pkgs.rustPlatform.fetchCargoVendor (x: {
          src = src x;
          hash = hash.${x};
        })
      else
        null;
    fetchCargoTarball =
      if pkgs.rustPlatform ? fetchCargoTarball && !(pkgs.rustPlatform ? fetchCargoVendor) then
        fetch pkgs.rustPlatform.fetchCargoTarball (x: {
          src = src x;
          name = "nfi-probe-${x}";
          hash = hash.${x};
        })
      else
        null;
    goModules = fetch (args: (pkgs.buildGoModule args).goModules) (x: {
      pname = "nfi-probe-${x}";
      version = "0";
      src = src x;
      vendorHash = hash.${x};
    });
    fetchNpmDeps = fetch pkgs.fetchNpmDeps (x: {
      name = "nfi-probe-${x}";
      src = src x;
      hash = hash.${x};
    });
    fetchYarnDeps = fetch pkgs.fetchYarnDeps (x: {
      yarnLock = src x + "/yarn.lock";
      hash = hash.${x};
    });

    stdenv = {
      kind = "target";
      targets = [
        pkgs.stdenv
        pkgs.stdenvNoCC
      ];
    };
  };
in
rec {
  probes' = lib.filterAttrs (_: p: p != null) probes;
  # Probe name to kind, cheap to evaluate: the probes themselves are only
  # instantiated one at a time.
  kinds = lib.mapAttrs (_: p: p.kind) probes';
}
