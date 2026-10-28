# One binary from the workspace package, exposed on its own (with its
# wrapper, if it has one).
{
  runCommand,
  nix-fetch-indexer,
  name,
}:

runCommand name { meta.mainProgram = name; } ''
  mkdir -p $out/bin
  ln -s ${nix-fetch-indexer}/bin/${name} $out/bin/${name}
  if [ -e ${nix-fetch-indexer}/bin/.${name}-wrapped ]; then
    ln -s ${nix-fetch-indexer}/bin/.${name}-wrapped $out/bin/.${name}-wrapped
  fi
''
