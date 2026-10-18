#!/usr/bin/env bash
# Print the settings of every channel to index, one JSON object per line.
#
# Usage: ci/channels.sh [channel]
#
# Channels are discovered from releases.nixos.org (`nfi-indexer list-channels`)
# and configured by the policy in ci/config.json:
#   nixos-YY.MM    stable channels from `oldest` onwards. Each gets cumulative
#                  bundles; channels from `per_release_from` onwards also get
#                  per-release bundles. Older channels were already finished
#                  when publishing started, so per-release bundles would only
#                  be a backfill nobody asked for.
#   nixos-unstable per-release bundles only, starting from `unstable_start`
#                  ("latest": the newest release at the first run).
# Small channels (-small, -aarch64) are skipped.
#
# Environment: NFI_INDEXER, NFI_CONFIG (see index-channel.sh).
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
NFI=${NFI_INDEXER:-nfi-indexer}
CONFIG=${NFI_CONFIG:-$here/config.json}

"$NFI" list-channels nixos | jq -cR --slurpfile cfg "$CONFIG" --arg only "${1:-}" '
  $cfg[0] as $c
  | if test("^[0-9]{2}\\.[0-9]{2}$") and . >= $c.oldest then
      {channel: "nixos-\(.)", prefix: "nixos/\(.)", series: ., cumulative: true,
       per_release: (. >= $c.per_release_from), start: "all"}
    elif . == "unstable" then
      {channel: "nixos-unstable", prefix: "nixos/unstable", series: "unstable",
       cumulative: false, per_release: true, start: ($c.unstable_start // "latest")}
    else empty end
  | select($only == "" or .channel == $only)'
