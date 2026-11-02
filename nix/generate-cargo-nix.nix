# `nix run .#generate-cargo-nix`: (re)generates nix/Cargo.nix, which
# package.nix imports, from Cargo.lock. Run it after cloning and after
# changing dependencies or workspace members.
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
    # The file is gitignored, and flakes only see files git knows about:
    # record it as intent-to-add so `nix build .` can import it without its
    # contents being staged.
    git add --intent-to-add --force nix/Cargo.nix
  '';
}
