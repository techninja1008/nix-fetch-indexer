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
channels=()
while read -r cfg; do
  channel=$(jq -r .channel <<<"$cfg")
  n=$(NFI_WORK=$(mktemp -d -p "$tmp") "$here/index-channel.sh" --check "$channel")
  echo "[$channel] $n new release(s)" >&2
  ((n > 0)) && channels+=("$channel")
done < <("$here/channels.sh" "${1:-}")
if ((${#channels[@]})); then
  printf '%s\n' "${channels[@]}" | jq -cRn '[inputs]'
else
  echo '[]'
fi
