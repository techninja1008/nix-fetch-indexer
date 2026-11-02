# `nix run .#generate-cargo-nix`: regenerates nix/Cargo.nix, which package.nix
# imports, from Cargo.lock. The pre-commit hook runs it whenever a Cargo.toml or
# Cargo.lock changes, and CI checks that the committed file is up to date.
{
  writeShellApplication,
  cargo,
  crate2nix,
  git,
  rustc,
}:

writeShellApplication {
  name = "generate-cargo-nix";
  runtimeInputs = [
    cargo
    crate2nix
    git
    rustc
  ];
  text = ''
    cd "$(git rev-parse --show-toplevel)"
    crate2nix generate -f Cargo.toml -o nix/Cargo.nix
  '';
}
