#!/usr/bin/env bash
# Index new releases of one channel and publish the bundles.
#
# Usage: ci/index-channel.sh <channel>        (e.g. nixos-26.05)
#
# The channel's settings come from ci/channels.json:
#   prefix       release prefix on releases.nixos.org (e.g. nixos/26.05)
#   series       short name used for cumulative assets (e.g. 26.05)
#   per_release  publish each release as its own GitHub release (tag = release
#                name) with one asset per dataset: <release>-<dataset>.nfib
#   cumulative   maintain <series>-<dataset>-cumulative.nfib assets on the
#                channel's GitHub release (tag = channel name)
#   start        "all" to backfill every release on first run, "latest" to
#                start from the newest release
#
# The channel's GitHub release also holds releases.txt: the releases processed
# so far (oldest first). Each new release is indexed with the previous
# release's closure as the seed, so only new narinfos are fetched.
#
# Environment:
#   NFI_STORE         gh:<owner>/<repo> (GitHub releases, needs GH_TOKEN) or
#                     dir:<path> (local directory, for testing)
#   NFI_INDEXER       nfi-indexer binary (default: nfi-indexer on PATH)
#   NFI_CHANNELS      channel config (default: channels.json next to this script)
#   NFI_DATASETS      default: closure,packages,programs
#   NFI_MAX_RELEASES  index at most this many new releases per run (default: all)
#   NFI_INDEX_ARGS    extra arguments for `nfi-indexer all` (e.g. --sample 50)
#   NFI_WORK          work directory (default: a temporary directory)
set -euo pipefail

channel=${1:?usage: index-channel.sh <channel>}
here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
NFI=${NFI_INDEXER:-nfi-indexer}
CHANNELS=${NFI_CHANNELS:-$here/channels.json}
DATASETS=${NFI_DATASETS:-closure,packages,programs}
MAX=${NFI_MAX_RELEASES:-0}
STORE=${NFI_STORE:?set NFI_STORE to gh:<owner>/<repo> or dir:<path>}
read -r -a INDEX_ARGS <<<"${NFI_INDEX_ARGS:-}"
IFS=, read -r -a datasets <<<"$DATASETS"

log() { echo "[$channel] $*" >&2; }

cfg=$(jq -ec --arg c "$channel" '.[] | select(.channel == $c)' "$CHANNELS") ||
  { echo "channel $channel not found in $CHANNELS" >&2; exit 2; }
prefix=$(jq -r .prefix <<<"$cfg")
series=$(jq -r .series <<<"$cfg")
per_release=$(jq -r '.per_release // false' <<<"$cfg")
cumulative=$(jq -r '.cumulative // false' <<<"$cfg")
start=$(jq -r '.start // "all"' <<<"$cfg")

work=${NFI_WORK:-$(mktemp -d)}
mkdir -p "$work"

# ---------- storage backends ----------
# store_get <tag> <asset> <dest>: download; fails if the asset doesn't exist.
# store_put <tag> <title> <notes> <file>...: create the release if needed and
#   upload (replacing) the files.
case $STORE in
  gh:*)
    repo=${STORE#gh:}
    store_get() { gh release download "$1" -R "$repo" -p "$2" -O "$3" --clobber >/dev/null 2>&1; }
    store_put() {
      local tag=$1 title=$2 notes=$3; shift 3
      if gh release view "$tag" -R "$repo" >/dev/null 2>&1; then
        gh release edit "$tag" -R "$repo" --title "$title" --notes "$notes" >/dev/null
      else
        gh release create "$tag" -R "$repo" --title "$title" --notes "$notes" --latest=false >/dev/null
      fi
      gh release upload "$tag" -R "$repo" --clobber "$@"
    }
    ;;
  dir:*)
    root=${STORE#dir:}
    store_get() { [[ -f $root/$1/$2 ]] && cp "$root/$1/$2" "$3"; }
    store_put() {
      local tag=$1 title=$2 notes=$3; shift 3
      mkdir -p "$root/$tag"
      printf '%s\n\n%s\n' "$title" "$notes" >"$root/$tag/NOTES"
      cp "$@" "$root/$tag/"
    }
    ;;
  *) echo "unknown NFI_STORE $STORE" >&2; exit 2 ;;
esac

# ---------- work out what is new ----------
manifest=$work/releases.txt
store_get "$channel" releases.txt "$manifest" || : >"$manifest"
last=$(tail -n1 "$manifest")
"$NFI" list-releases "$prefix" ${last:+--since "$last"} >"$work/listing.tsv"
# Drop anything already processed (guards against odd revision orderings).
grep -vFf <(sed 's/$/\t/' "$manifest") "$work/listing.tsv" >"$work/new.tsv" || true
if [[ -z $last && $start == latest ]]; then
  tail -n1 "$work/new.tsv" >"$work/new.tmp" && mv "$work/new.tmp" "$work/new.tsv"
fi
if ((MAX > 0)); then
  head -n "$MAX" "$work/new.tsv" >"$work/new.tmp" && mv "$work/new.tmp" "$work/new.tsv"
fi
count=$(wc -l <"$work/new.tsv")
if ((count == 0)); then
  log "up to date (last: ${last:-none})"
  exit 0
fi
log "$count new release(s) after ${last:-<start>}: $(cut -f1 "$work/new.tsv" | tr '\n' ' ')"

notes_for() { # <bundle> -> markdown summary of its releases and sections
  "$NFI" inspect --json "$1" | jq -r '
    "| release | commit | closure paths | packages | programs |",
    "|---|---|---:|---:|---:|",
    (.releases[] | "| \(.name) | `\(.commit)` | \(.closure_paths) | \(.packages) | \(.programs) |")'
}

# ---------- seed: the previous release's closure ----------
seed=""
if [[ -n $last ]]; then
  if [[ $per_release == true ]] && store_get "$last" "$last-closure.nfib" "$work/seed.nfib"; then
    seed=$work/seed.nfib
  elif [[ $cumulative == true ]] && store_get "$channel" "$series-closure-cumulative.nfib" "$work/cum-closure.nfib"; then
    "$NFI" filter "$work/cum-closure.nfib" --release "$last" --section closure -o "$work/seed.nfib"
    seed=$work/seed.nfib
  fi
  [[ -n $seed ]] && log "seeding from $last" || log "no seed available for $last; indexing from scratch"
fi

# ---------- index ----------
declare -A new_parts # dataset -> space-separated bundle files to fold into the cumulative
if [[ $per_release == true ]]; then
  while IFS=$'\t' read -r name url; do
    files=()
    have_all=true
    for ds in "${datasets[@]}"; do
      f=$work/$name-$ds.nfib
      files+=("$f")
      store_get "$name" "$name-$ds.nfib" "$f" || have_all=false
    done
    if [[ $have_all == true ]]; then
      log "$name already published; reusing its bundles"
    else
      log "indexing $name"
      "$NFI" all --release-url "$url" --datasets "$DATASETS" ${seed:+--seed "$seed"} \
        "${INDEX_ARGS[@]}" -o "$work/$name.nfib"
      for ds in "${datasets[@]}"; do
        "$NFI" filter "$work/$name.nfib" --section "$ds" -o "$work/$name-$ds.nfib"
      done
      store_put "$name" "$name" "$(notes_for "$work/$name.nfib")" "${files[@]}"
      rm -f "$work/$name.nfib"
    fi
    for ds in "${datasets[@]}"; do new_parts[$ds]+=" $work/$name-$ds.nfib"; done
    # The next release is seeded from this one.
    seed=$work/$name-closure.nfib
  done <"$work/new.tsv"
else
  urls=()
  while IFS=$'\t' read -r name url; do urls+=(--release-url "$url"); done <"$work/new.tsv"
  log "indexing $count release(s)"
  "$NFI" all "${urls[@]}" --datasets "$DATASETS" ${seed:+--seed "$seed"} "${INDEX_ARGS[@]}" -o "$work/new.nfib"
  for ds in "${datasets[@]}"; do
    "$NFI" filter "$work/new.nfib" --section "$ds" -o "$work/new-$ds.nfib"
    new_parts[$ds]=" $work/new-$ds.nfib"
  done
  rm -f "$work/new.nfib"
fi

# ---------- cumulative ----------
if [[ $cumulative == true ]]; then
  cum_files=()
  for ds in "${datasets[@]}"; do
    cum=$work/$series-$ds-cumulative.nfib
    inputs=()
    if [[ -f $work/cum-$ds.nfib ]] || store_get "$channel" "$series-$ds-cumulative.nfib" "$work/cum-$ds.nfib"; then
      inputs+=("$work/cum-$ds.nfib")
    fi
    read -r -a parts <<<"${new_parts[$ds]}"
    log "updating $series-$ds-cumulative.nfib (${#inputs[@]} existing + ${#parts[@]} new)"
    "$NFI" merge "${inputs[@]}" "${parts[@]}" -o "$cum"
    cum_files+=("$cum")
  done
  cut -f1 "$work/new.tsv" >>"$manifest"
  title="$channel (cumulative)"
  notes="Cumulative index of $channel: $(wc -l <"$manifest") releases, $(head -n1 "$manifest") to $(tail -n1 "$manifest")."
  store_put "$channel" "$title" "$notes" "${cum_files[@]}" "$manifest"
else
  cut -f1 "$work/new.tsv" >>"$manifest"
  store_put "$channel" "$channel" "Releases of $channel indexed so far: see releases.txt." "$manifest"
fi
log "done: $(wc -l <"$manifest") release(s) recorded"
