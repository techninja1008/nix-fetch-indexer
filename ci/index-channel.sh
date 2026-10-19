#!/usr/bin/env bash
# Index new releases of one channel and publish the bundles. Nothing is
# ingested anywhere: the bundles are only uploaded.
#
# Usage: ci/index-channel.sh [--check] <channel>    (e.g. nixos-26.05)
#
#   --check  only print the number of new releases, and exit
#
# The channel's settings come from ci/channels.sh (policy: ci/config.json):
#   per_release  publish each release as its own GitHub release (tag = release
#                name) with one asset per dataset: <release>-<dataset>.nfib
#
# Datasets are the indexer's sections, except that closure sections are split
# by closure type: `closure` holds the store-paths closures, `helpers-direct`
# and `helpers-full` the helper probe closures (indexing them needs Nix).
# `eval-errors` holds the helper probes that fail to evaluate (also needs Nix).
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
#   NFI_CONFIG        policy (default: config.json next to this script)
#   NFI_DATASETS      default: `datasets` from the policy
#   NFI_MAX_RELEASES  index at most this many new releases per run (default: all)
#   NFI_BACKFILL_DATASETS
#                     instead of indexing new releases, add these datasets (e.g.
#                     helpers-direct,helpers-full,eval-errors) to releases already recorded
#                     that lack them
#   NFI_INDEX_ARGS    extra arguments for `nfi-indexer all` (e.g. --sample 50)
#   NFI_WORK          work directory (default: a temporary directory)
set -euo pipefail

check=false
if [[ ${1:-} == --check ]]; then check=true; shift; fi
channel=${1:?usage: index-channel.sh [--check] <channel>}
here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
NFI=${NFI_INDEXER:-nfi-indexer}
export NFI_CONFIG=${NFI_CONFIG:-$here/config.json}
DATASETS=${NFI_DATASETS:-$(jq -r '.datasets // "closure,packages,programs"' "$NFI_CONFIG")}
BACKFILL=${NFI_BACKFILL_DATASETS:-}
[[ -n $BACKFILL ]] && DATASETS=$BACKFILL
MAX=${NFI_MAX_RELEASES:-0}
STORE=${NFI_STORE:?set NFI_STORE to gh:<owner>/<repo> or dir:<path>}
read -r -a INDEX_ARGS <<<"${NFI_INDEX_ARGS:-}"
IFS=, read -r -a datasets <<<"$DATASETS"

log() { echo "[$channel] $*" >&2; }

# Published dataset -> indexer section and closure type.
ds_section() { case $1 in closure | helpers-*) echo closure ;; *) echo "$1" ;; esac; }
ds_closure_type() { case $1 in closure) echo store-paths ;; helpers-*) echo "$1" ;; *) echo "" ;; esac; }
sections=() closure_types=() closure_datasets=()
for ds in "${datasets[@]}"; do
  sec=$(ds_section "$ds")
  [[ " ${sections[*]} " == *" $sec "* ]] || sections+=("$sec")
  ct=$(ds_closure_type "$ds")
  [[ -n $ct ]] && closure_types+=("$ct") closure_datasets+=("$ds")
done
INDEX_DATASETS=$(IFS=,; echo "${sections[*]}")
CLOSURE_TYPES=$(IFS=,; echo "${closure_types[*]}")
# filter_ds <bundle> <dataset> <out>: the dataset's part of a bundle.
filter_ds() {
  local ct
  ct=$(ds_closure_type "$2")
  "$NFI" filter "$1" --section "$(ds_section "$2")" ${ct:+--closure-type "$ct"} -o "$3"
}
# index <out> <url>...: index releases into one bundle (seeded from $seed).
index() {
  local out=$1 url args=()
  shift
  for url in "$@"; do args+=(--release-url "$url"); done
  "$NFI" all "${args[@]}" --datasets "$INDEX_DATASETS" ${CLOSURE_TYPES:+--closure-types "$CLOSURE_TYPES"} \
    ${seed:+--seed "$seed"} "${INDEX_ARGS[@]}" -o "$out"
}

cfg=$(NFI_INDEXER=$NFI "$here/channels.sh" "$channel")
[[ -n $cfg ]] || { echo "channel $channel is not configured (see ci/channels.sh)" >&2; exit 2; }
prefix=$(jq -r .prefix <<<"$cfg")
series=$(jq -r .series <<<"$cfg")
per_release=$(jq -r .per_release <<<"$cfg")
cumulative=$(jq -r .cumulative <<<"$cfg")
start=$(jq -r .start <<<"$cfg")

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

# ---------- work out what to index ----------
manifest=$work/releases.txt
store_get "$channel" releases.txt "$manifest" || : >"$manifest"
last=$(tail -n1 "$manifest")
if [[ -n $BACKFILL ]]; then
  # Recorded releases lacking a dataset: per-release assets, or (channels
  # without them) closures/sections of the cumulative bundles.
  "$NFI" list-releases "$prefix" >"$work/listing.tsv"
  grep -Ff <(sed 's/$/\t/' "$manifest") "$work/listing.tsv" >"$work/recorded.tsv" || true
  : >"$work/new.tsv"
  if [[ $per_release == true ]]; then
    while IFS=$'\t' read -r name url; do
      for ds in "${datasets[@]}"; do
        if ! store_get "$name" "$name-$ds.nfib" /dev/null; then
          printf '%s\t%s\n' "$name" "$url" >>"$work/new.tsv"
          break
        fi
      done
    done <"$work/recorded.tsv"
  else
    : >"$work/have.txt"
    first=true
    for ds in "${datasets[@]}"; do
      if store_get "$channel" "$series-$ds-cumulative.nfib" "$work/cum-$ds.nfib"; then
        ct=$(ds_closure_type "$ds")
        "$NFI" inspect --json "$work/cum-$ds.nfib" | jq -r --arg ct "$ct" \
          'if $ct == "" then .releases[].name else .closures[] | select(.closure_type == $ct) | .release end' \
          | sort -u >"$work/have-$ds.txt"
      else
        : >"$work/have-$ds.txt"
      fi
      # Releases having every dataset already.
      if [[ $first == true ]]; then cp "$work/have-$ds.txt" "$work/have.txt"; first=false; else
        comm -12 "$work/have.txt" "$work/have-$ds.txt" >"$work/have.tmp" && mv "$work/have.tmp" "$work/have.txt"
      fi
    done
    grep -vFf <(sed 's/$/\t/' "$work/have.txt") "$work/recorded.tsv" >"$work/new.tsv" || true
  fi
else
  "$NFI" list-releases "$prefix" ${last:+--since "$last"} >"$work/listing.tsv"
  # Drop anything already processed (guards against odd revision orderings).
  grep -vFf <(sed 's/$/\t/' "$manifest") "$work/listing.tsv" >"$work/new.tsv" || true
  if [[ -z $last && $start == latest ]]; then
    tail -n1 "$work/new.tsv" >"$work/new.tmp" && mv "$work/new.tmp" "$work/new.tsv"
  fi
fi
if ((MAX > 0)); then
  head -n "$MAX" "$work/new.tsv" >"$work/new.tmp" && mv "$work/new.tmp" "$work/new.tsv"
fi
count=$(wc -l <"$work/new.tsv")
if [[ $check == true ]]; then
  echo "$count"
  exit 0
fi
if ((count == 0)); then
  log "up to date (last: ${last:-none}${BACKFILL:+; backfill $BACKFILL})"
  exit 0
fi
if [[ -n $BACKFILL ]]; then
  log "backfilling $BACKFILL for $count release(s): $(cut -f1 "$work/new.tsv" | tr '\n' ' ')"
else
  log "$count new release(s) after ${last:-<start>}: $(cut -f1 "$work/new.tsv" | tr '\n' ' ')"
fi

notes_for() { # <bundle> -> markdown summary of its releases, closures and evals
  "$NFI" inspect --json "$1" | jq -r '
    "| release | commit | closure paths | packages | programs |",
    "|---|---|---:|---:|---:|",
    (.releases[] | "| \(.name) | `\(.commit)` | \(.closure_paths) | \(.packages) | \(.programs) |"),
    if (.closures | length) > 0 then
      "", "| closure | paths | missing |", "|---|---:|---:|",
      (.closures[] | "| \(.release) \(.closure_type) | \(.paths) | \(.missing) |")
    else empty end,
    if ((.evals // []) | length) > 0 then
      "", "| eval | errors |", "|---|---:|",
      (.evals[] | "| \(.release) \(.eval_type) | \(.errors) |")
    else empty end'
}

# ---------- seed: the previous release's closures ----------
# Releases share almost all of their closures with the previous release in
# the channel, so its closures (of every closure type) seed the indexer.
seed=""
seed_from=$last
if [[ -n $BACKFILL ]]; then
  # The release before the first one to backfill, if any.
  first_new=$(head -n1 "$work/new.tsv" | cut -f1)
  seed_from=$(grep -B1 -xF "$first_new" "$manifest" | head -n1)
  [[ $seed_from == "$first_new" ]] && seed_from=""
fi
if [[ -n $seed_from && ${#closure_datasets[@]} -gt 0 ]]; then
  parts=()
  for ds in "${closure_datasets[@]}"; do
    f=$work/seed-$ds.nfib
    if [[ $per_release == true ]] && store_get "$seed_from" "$seed_from-$ds.nfib" "$f"; then
      parts+=("$f")
    elif [[ $cumulative == true ]] && { [[ -f $work/cum-$ds.nfib ]] ||
      store_get "$channel" "$series-$ds-cumulative.nfib" "$work/cum-$ds.nfib"; }; then
      "$NFI" filter "$work/cum-$ds.nfib" --release "$seed_from" --section closure -o "$f"
      parts+=("$f")
    fi
  done
  if ((${#parts[@]})); then
    "$NFI" merge "${parts[@]}" -o "$work/seed.nfib"
    seed=$work/seed.nfib
    log "seeding from $seed_from"
  else
    log "no seed available for $seed_from; indexing from scratch"
  fi
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
      next_seed=()
      for ds in "${closure_datasets[@]}"; do next_seed+=("$work/$name-$ds.nfib"); done
      if ((${#next_seed[@]})); then
        "$NFI" merge "${next_seed[@]}" -o "$work/$name.nfib"
      fi
    else
      log "indexing $name"
      index "$work/$name.nfib" "$url"
      for ds in "${datasets[@]}"; do filter_ds "$work/$name.nfib" "$ds" "$work/$name-$ds.nfib"; done
      store_put "$name" "$name" "$(notes_for "$work/$name.nfib")" "${files[@]}"
    fi
    for ds in "${datasets[@]}"; do new_parts[$ds]+=" $work/$name-$ds.nfib"; done
    # The next release is seeded from this one (all of its closures).
    [[ -n $seed && $seed == "$work/"*"-full.nfib" ]] && rm -f "$seed"
    if [[ -f $work/$name.nfib ]]; then
      mv "$work/$name.nfib" "$work/$name-full.nfib"
      seed=$work/$name-full.nfib
    fi
  done <"$work/new.tsv"
else
  urls=()
  while IFS=$'\t' read -r name url; do urls+=("$url"); done <"$work/new.tsv"
  log "indexing $count release(s)"
  index "$work/new.nfib" "${urls[@]}"
  for ds in "${datasets[@]}"; do
    filter_ds "$work/new.nfib" "$ds" "$work/new-$ds.nfib"
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
  [[ -n $BACKFILL ]] || cut -f1 "$work/new.tsv" >>"$manifest"
  title="$channel (cumulative)"
  notes="Cumulative index of $channel: $(wc -l <"$manifest") releases, $(head -n1 "$manifest") to $(tail -n1 "$manifest")."
  store_put "$channel" "$title" "$notes" "${cum_files[@]}" "$manifest"
elif [[ -z $BACKFILL ]]; then
  cut -f1 "$work/new.tsv" >>"$manifest"
  store_put "$channel" "$channel" "Releases of $channel indexed so far: see releases.txt." "$manifest"
fi
log "done: $(wc -l <"$manifest") release(s) recorded"
