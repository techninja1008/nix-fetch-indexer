#!/usr/bin/env bash
# Print a JSON array of the channels that have new releases to index.
#
# Usage: ci/plan.sh [channel]
#
# Environment: as for index-channel.sh.
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
# Not `done < <(channels.sh)`: a failing process substitution goes unnoticed,
# leaving an empty plan and a green run.
configs=$("$here/channels.sh" "${1:-}")
channels=()
while read -r cfg; do
  [[ -n $cfg ]] || continue
  channel=$(jq -r .channel <<<"$cfg")
  n=$(NFI_WORK=$(mktemp -d -p "$tmp") "$here/index-channel.sh" --check "$channel")
  echo "[$channel] $n new release(s)" >&2
  if ((n > 0)); then channels+=("$channel"); fi
done <<<"$configs"
if ((${#channels[@]})); then
  printf '%s\n' "${channels[@]}" | jq -cRn '[inputs]'
else
  echo '[]'
fi
