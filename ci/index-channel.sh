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
#                name) with one asset per output: <release>-<output>.nfib
#   cumulative   maintain <series>-<output>-cumulative.nfib assets on the
#                channel's GitHub release (tag = channel name)
#   start        "all" to backfill every release on first run, "latest" to
#                start from the newest release
#
# What is indexed is given as indexer targets (`nfi-indexer index --targets`),
# e.g. store-paths,helpers:direct,helpers:full,packages,programs. Each release
# is indexed in one run for all targets, then split into outputs, one file
# each:
#   - one per closure type: `store-paths`, `helpers-direct`, `helpers-full`
#     (`<eval type>[-<closure type>]`; the helpers ones need Nix);
#   - one per evaluator that records eval errors: `helpers-eval-errors`;
#   - one per other target: `packages`, `programs`, `outpaths`.
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
#   NFI_TARGETS       default: `targets` from the policy
#   NFI_MAX_RELEASES  index at most this many new releases per run (default: all)
#   NFI_BACKFILL_TARGETS
#                     instead of indexing new releases, index these targets
#                     (e.g. helpers:direct,helpers:full) for releases already
#                     recorded that lack any of their outputs
#   NFI_INDEX_ARGS    extra arguments for `nfi-indexer index` (e.g. --sample 50)
#   NFI_WORK          work directory (default: a temporary directory)
set -euo pipefail

check=false
if [[ ${1:-} == --check ]]; then check=true; shift; fi
channel=${1:?usage: index-channel.sh [--check] <channel>}
here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
NFI=${NFI_INDEXER:-nfi-indexer}
export NFI_CONFIG=${NFI_CONFIG:-$here/config.json}
POLICY_TARGETS=${NFI_TARGETS:-$(jq -r '.targets // "store-paths,packages,programs"' "$NFI_CONFIG")}
BACKFILL=${NFI_BACKFILL_TARGETS:-}
TARGETS=${BACKFILL:-$POLICY_TARGETS}
MAX=${NFI_MAX_RELEASES:-0}
STORE=${NFI_STORE:?set NFI_STORE to gh:<owner>/<repo> or dir:<path>}
read -r -a INDEX_ARGS <<<"${NFI_INDEX_ARGS:-}"
IFS=, read -r -a targets <<<"$TARGETS"

log() { echo "[$channel] $*" >&2; }

# Closure type targets, and the evaluators that record eval errors.
closure_targets=" store-paths helpers:direct helpers:full "
records_errors() { [[ $1 == helpers ]]; }

# output_lines <target>...: "<name> <kind> <selector>" for each output of the
# targets: one per closure type (kind closure, selected by closure type), one
# per evaluator that records eval errors (eval-errors, by evaluator) and one
# per other target (section).
output_lines() {
  local t
  for t; do
    case $t in
      packages | programs | outpaths) echo "$t section $t" ;;
      *)
        if [[ $closure_targets != *" $t "* ]]; then
          echo "unknown target $t (expected:$closure_targets""packages programs outpaths)" >&2
          return 1
        fi
        echo "${t/:/-} closure $t"
        if records_errors "${t%%:*}"; then echo "${t%%:*}-eval-errors eval-errors ${t%%:*}"; fi
        ;;
    esac
  done
}
# outputs (and closure_outputs, out_kind, out_sel): this run's outputs.
# policy_outputs: the outputs of the policy's targets, which a release's
# notes cover.
outputs=() closure_outputs=() policy_outputs=()
declare -A out_kind out_sel
lines=$(output_lines "${targets[@]}") || exit 2
while read -r name kind sel; do
  [[ -n ${out_kind[$name]:-} ]] && continue
  outputs+=("$name") out_kind[$name]=$kind out_sel[$name]=$sel
  if [[ $kind == closure ]]; then closure_outputs+=("$name"); fi
done <<<"$lines"
IFS=, read -r -a policy_targets <<<"$POLICY_TARGETS"
lines=$(output_lines "${policy_targets[@]}") || exit 2
while read -r name _; do
  if [[ " ${policy_outputs[*]} " != *" $name "* ]]; then policy_outputs+=("$name"); fi
done <<<"$lines"
# filter_out <bundle> <output> <file>: the output's part of a bundle.
filter_out() {
  case ${out_kind[$2]} in
    closure) "$NFI" filter "$1" --section closure --closure-type "${out_sel[$2]}" -o "$3" ;;
    eval-errors) "$NFI" filter "$1" --section eval-errors --eval-type "${out_sel[$2]}" -o "$3" ;;
    section) "$NFI" filter "$1" --section "${out_sel[$2]}" -o "$3" ;;
  esac
}
# index <out> <url>...: index releases into one bundle (seeded from $seed).
index() {
  local out=$1 url args=()
  shift
  for url in "$@"; do args+=(--release-url "$url"); done
  "$NFI" index "${args[@]}" --targets "$TARGETS" ${seed:+--seed "$seed"} "${INDEX_ARGS[@]}" -o "$out"
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
  # Recorded releases lacking an output: per-release assets, or (channels
  # without them) closures/sections of the cumulative bundles.
  "$NFI" list-releases "$prefix" >"$work/listing.tsv"
  grep -Ff <(sed 's/$/\t/' "$manifest") "$work/listing.tsv" >"$work/recorded.tsv" || true
  : >"$work/new.tsv"
  if [[ $per_release == true ]]; then
    while IFS=$'\t' read -r name url; do
      for ds in "${outputs[@]}"; do
        if ! store_get "$name" "$name-$ds.nfib" /dev/null; then
          printf '%s\t%s\n' "$name" "$url" >>"$work/new.tsv"
          break
        fi
      done
    done <"$work/recorded.tsv"
  else
    : >"$work/have.txt"
    first=true
    for ds in "${outputs[@]}"; do
      if store_get "$channel" "$series-$ds-cumulative.nfib" "$work/cum-$ds.nfib"; then
        "$NFI" inspect --json "$work/cum-$ds.nfib" |
          jq -r --arg kind "${out_kind[$ds]}" --arg sel "${out_sel[$ds]}" '
            if $kind == "closure" then .closures[] | select(.closure_type == $sel) | .release
            elif $kind == "eval-errors" then .evals[] | select(.eval_type == $sel and .errors != null) | .release
            else .releases[].name end' \
          | sort -u >"$work/have-$ds.txt"
      else
        : >"$work/have-$ds.txt"
      fi
      # Releases having every output already.
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
    if ([(.evals // [])[] | select(.errors != null)] | length) > 0 then
      "", "| evaluation | errors |", "|---|---:|",
      (.evals[] | select(.errors != null) | "| \(.release) \(.eval_type) | \(.errors) |")
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
if [[ -n $seed_from && ${#closure_outputs[@]} -gt 0 ]]; then
  parts=()
  for ds in "${closure_outputs[@]}"; do
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
declare -A new_parts # output -> space-separated bundle files to fold into the cumulative
if [[ $per_release == true ]]; then
  while IFS=$'\t' read -r name url; do
    files=()
    have_all=true
    for ds in "${outputs[@]}"; do
      f=$work/$name-$ds.nfib
      files+=("$f")
      store_get "$name" "$name-$ds.nfib" "$f" || have_all=false
    done
    if [[ $have_all == true ]]; then
      log "$name already published; reusing its bundles"
      next_seed=()
      for ds in "${closure_outputs[@]}"; do next_seed+=("$work/$name-$ds.nfib"); done
      if ((${#next_seed[@]})); then
        "$NFI" merge "${next_seed[@]}" -o "$work/$name.nfib"
      fi
    else
      log "indexing $name"
      index "$work/$name.nfib" "$url"
      for ds in "${outputs[@]}"; do filter_out "$work/$name.nfib" "$ds" "$work/$name-$ds.nfib"; done
      # The notes cover all of the release's outputs: when backfilling, the
      # ones already published too.
      others=()
      for ds in "${policy_outputs[@]}"; do
        if [[ -z ${out_kind[$ds]:-} ]] && store_get "$name" "$name-$ds.nfib" "$work/$name-$ds.nfib"; then
          others+=("$work/$name-$ds.nfib")
        fi
      done
      notes_src=$work/$name.nfib
      if ((${#others[@]})); then
        "$NFI" merge "$work/$name.nfib" "${others[@]}" -o "$work/$name-notes.nfib"
        notes_src=$work/$name-notes.nfib
      fi
      store_put "$name" "$name" "$(notes_for "$notes_src")" "${files[@]}"
      rm -f "$work/$name-notes.nfib"
    fi
    for ds in "${outputs[@]}"; do new_parts[$ds]+=" $work/$name-$ds.nfib"; done
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
  for ds in "${outputs[@]}"; do
    filter_out "$work/new.nfib" "$ds" "$work/new-$ds.nfib"
    new_parts[$ds]=" $work/new-$ds.nfib"
  done
  rm -f "$work/new.nfib"
fi

# ---------- cumulative ----------
if [[ $cumulative == true ]]; then
  cum_files=()
  for ds in "${outputs[@]}"; do
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
