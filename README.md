# nix-fetch-indexer

Tooling for indexing the binary-cache closure of NixOS / nixpkgs releases. The index answers queries like:

- **Minimise:** which cached store paths are worth substituting to build this flake, and which derivations still have to be built?
- **Bulk lookup:** which store paths, and which closure, belong to these `attr@commit` pairs?
- **Search:** full-text search over packages and the programs they provide, filterable by attribute, version, program, system, release, channel, series and commit.

See [DESIGN.md](DESIGN.md) for the full design.

## Components

| binary | crate | purpose |
|---|---|---|
| `nfi-indexer` | `crates/nfi-indexer` | Builds interchange bundles from releases.nixos.org and cache.nixos.org, and provides offline bundle tooling |
| `nfi-service` | `crates/nfi-service` | Query service: Postgres plus Meilisearch, with a public HTTP/JSON API and a separate admin listener |
| `nfi` | `crates/nfi` | Query CLI: `search`, `show`, `bulk`, `minimise` (with fetch hooks) |
| `nfi-admin` | `crates/nfi-admin` | Admin CLI: `ingest`, `jobs`, `releases`, `prune`, `reindex` |

Supporting libraries:

- `nfi-interchange`: the bundle format.
- `nfi-api`: shared API types, the derivation-graph reducer, the minimise planner and an HTTP client.
- `nfi-embedded-postgres`, `nfi-embedded-meilisearch`: supervised child-process PostgreSQL and Meilisearch for single-node mode.
- `nfi-testenv`: ephemeral Postgres and Meilisearch for tests.

## Building

```sh
nix build            # all binaries (runs the full test suite, including integration tests)
nix build .#nfi      # a single binary
nix build .#nfi-service-image && docker load < result   # OCI image (also .#nfi-indexer-image)
nix develop          # cargo, clippy, postgresql, meilisearch, nix-eval-jobs
cargo build --release
```

## Indexing

```sh
R=https://releases.nixos.org/nixos/25.05/nixos-25.05.813814.ac62194c3917

# Default targets: store-paths closures, packages and programs.
nfi-indexer index --release-url $R -o release.nfib
nfi-indexer index --release-url $R --targets store-paths -o closure.nfib   # store-paths.xz + narinfos
nfi-indexer index --release-url $R --targets packages,programs -o meta.nfib  # packages.json.br, programs.sqlite
nfi-indexer index --release-url $R --targets outpaths --systems x86_64-linux,aarch64-linux -o outpaths.nfib  # nix-eval-jobs

# Index every release of a channel, or only the newest few:
nfi-indexer index --channel-list nixos/25.05 --limit 3 -o recent.nfib

# Incremental: reuse a previous bundle's closures so known narinfos are not fetched again.
nfi-indexer index --release-url $R2 --targets store-paths --seed closure.nfib -o closure-2.nfib

# Build inputs of nixpkgs helpers (needs Nix), alongside the runtime closures:
nfi-indexer index --release-url $R --targets store-paths,helpers:direct,helpers:full -o closures.nfib
nfi-indexer index --release-url $R --targets helpers:direct -o helpers-direct.nfib
```

**Targets**

`--targets` names what to index. Datasets read from the release are `packages`, `programs` and `outpaths`. Closures are named by their closure type, `<evaluator>[:<closure type>]`: the **evaluator** that produces their roots and, within it, the closure type's name, which only `store-paths` leaves empty. An evaluator alone (`helpers`) isn't a target, since it has no closure type of its own. Closure types of one evaluator selected together share one evaluation.

**Evaluations and closures**

An **evaluation** is a release evaluated in one way; its **eval type** names the evaluator. Each evaluation produces one or more **closures**, one per closure type of its eval type. Closure type names are namespaced by eval type and may be empty. A closure belongs to its evaluation, and through it to the release. Closure types differ only in where their roots come from; the narinfo walk, seeding and missing-path tracking are shared. A path's runtime closure is always included.

| target | evaluator | roots |
|---|---|---|
| `store-paths` | `store-paths`: a pseudo-evaluation, since the paths come from Hydra's evaluation; one unnamed closure type | the paths listed in the release's `store-paths.xz`: outputs of Hydra jobs |
| `helpers:direct` | `helpers`: helper probes | outputs of the direct build inputs of nixpkgs helpers, e.g. fetchurl's own bootstrap `curl`, plus `stdenv` and `stdenvNoCC` |
| `helpers:full` | `helpers` | everything in those helpers' build graphs: bootstrap stages, compilers, and the cached sources they are built from |

Build-only paths like these are in no runtime closure of `store-paths.xz`, so without them minimise reports them as needing a build. The `helpers` evaluator runs **probes** ([`probes.nix`](crates/nfi-indexer/src/probes.nix)) once per release, for whichever of its closure types are selected:

- **Calls:** each probe calls a helper twice with every argument changed (URL, revision, hash, name, source) and instantiates both calls against the release's nixexprs. Only derivations shared by both calls count, so anything built from the probe's own arguments drops out.
- **Coverage:** fetchurl, fetchzip and fetchpatch; fetchgit (plain, with submodules, `leaveDotGit`, LFS and sparse checkout); fetchFromGitHub and fetchFromGitLab; fetchsvn, fetchhg, fetchbzr and fetchcvs; fetchCargoVendor (or fetchCargoTarball), buildGoModule's vendor derivation, fetchNpmDeps and fetchYarnDeps; and stdenv/stdenvNoCC as targets.
- **Versions:** probes check which helpers and arguments a nixpkgs version has, so the same file works from 24.05 to unstable. A probe for a helper or argument that a version lacks is left out: it doesn't apply, so nothing is missed.
- **Eval errors:** a probe that applies but fails to evaluate is an **eval error** of the release's `helpers` evaluation, identified by probe name and system only. On 24.05 and 26.05 every applicable probe evaluates.
- **Systems:** `--probe-systems` (default `x86_64-linux,aarch64-linux`). Probes need `nix-instantiate` and `nix` (`--nix-instantiate`, `--nix`); a release takes about 15–30 s to probe.

**Eval errors**

The `eval-errors` section records attributes that failed to evaluate, keyed by attribute and system, per evaluation. Evaluation is deterministic, so no message is kept. For each evaluation the section either holds its errors, possibly none, or says nothing about them. Eval errors only come from actually evaluating something, so they aren't a target: running an evaluator records its errors with its closures. The `store-paths` pseudo-evaluation evaluates nothing itself, so it records no error information.

**Indexer options**

- `--concurrency`: number of concurrent narinfo requests (default 512).
- `--connections`: number of independent HTTP/2 connections the requests are spread over (default 8). cache.nixos.org caps concurrent streams per connection at about 100, so throughput scales with this setting. In testing, 8×512 reached about 10k narinfos/s and 16×1024 about 14–20k/s. These limits apply to a shared public cache, so raise them with care.
- `--cache-url`: the binary cache to query (default `https://cache.nixos.org`).
- `--sample N`: index only the first N roots, for testing.
- `--format json`: write the JSON form.
- `--compression {none,zstd,xz,brotli}[:level]`: compression for the binary form (default `zstd:9`).
- `--v2-closure-type`: the closure type to read version 2 bundles (which predate closure types) as, e.g. `helpers:full`, for every command that reads bundles (default `store-paths`).

**Bundle tools**

```sh
nfi-indexer merge a.nfib b.nfib -o ab.nfib
nfi-indexer split ab.nfib -o per-release/
nfi-indexer filter ab.nfib --channel nixos-25.05 --section closure,packages -o f.nfib
nfi-indexer filter ab.nfib --section closure,eval-errors --closure-type helpers:direct -o helpers.nfib
nfi-indexer convert ab.nfib --format json -o ab.json
nfi-indexer inspect ab.nfib
nfi-indexer list-channels nixos   # 24.05, 26.05, unstable, ...
nfi-indexer list-releases nixos/26.05 --since nixos-26.05.590.ec942ba042da  # name<TAB>url, oldest first
nfi-indexer bench-compression ab.nfib
```

## Published indexes

The `Index releases` workflow (`.github/workflows/index-releases.yml`) runs every six hours, and can also be started by hand for one channel with a cap on how many new releases to index. It only uploads: bundles are published as GitHub release assets on [`techninja1008/nix-fetch-indexer-data`](https://github.com/techninja1008/nix-fetch-indexer-data), and nothing is ingested anywhere.

1. A plan job builds `nfi-indexer` once. [`ci/channels.sh`](ci/channels.sh) discovers the channels on releases.nixos.org, and [`ci/plan.sh`](ci/plan.sh) keeps those with releases not yet published.
2. One job per such channel runs [`ci/index-channel.sh`](ci/index-channel.sh), which indexes the new releases and uploads binary bundles. What is indexed is set as indexer targets (`targets` in [`ci/config.json`](ci/config.json): `store-paths,helpers:direct,helpers:full,packages,programs`). Each release is indexed in one run for all targets (both helper closure types share one probe run), and the result is split into one output per closure type (`store-paths`, `helpers-direct`, `helpers-full`), one per evaluator that records eval errors (`helpers-eval-errors`), and one per other target (`packages`, `programs`). The helper closures are separate outputs because `helpers-full` is much larger: it reaches the whole stdenv bootstrap.

**Channels.** Channels are discovered automatically according to the policy in [`ci/config.json`](ci/config.json); `-small` and `-aarch64` channels are skipped.

| Channel | Per-release bundles | Cumulative bundles | Starting point |
|---|---|---|---|
| nixos-unstable | yes | no | the newest release when first run (`unstable_start`) |
| nixos-YY.MM from 26.05 (`per_release_from`), including new channels such as 26.11 | yes | yes | every release |
| nixos-24.05 (`oldest`) to nixos-25.11 | no | yes | every release |

A stable channel only gets releases for about a month after the next one ships; 25.11's last release, for example, was on 2026-07-01. Channels before 26.05 had stopped updating before publishing started, so they only get cumulative bundles. Every release from now on, in any stable channel, gets both per-release and cumulative bundles.

**Assets**

- **Per-release bundles.** Release tag `<release>` (e.g. `nixos-26.05.590.ec942ba042da`). One asset per output: `<release>-store-paths.nfib`, `<release>-helpers-direct.nfib`, `<release>-helpers-full.nfib`, `<release>-helpers-eval-errors.nfib`, `<release>-packages.nfib` and `<release>-programs.nfib`. The release notes summarise path, package and program counts, each closure's paths and missing paths, and each evaluation's errors.
- **Cumulative bundles.** Release tag `<channel>` (e.g. `nixos-26.05`). One asset per output covering every release processed so far: `<series>-<output>-cumulative.nfib`, e.g. `26.05-store-paths-cumulative.nfib`.
- **Manifest.** Each channel's release (tag `<channel>`) also holds `releases.txt`, which lists the releases processed so far, oldest first. The next run starts after its last line.

**Seeding.** A release usually shares nearly all of its closures with the previous release in the same channel, and very little with older ones. Each new release is therefore seeded with the previous release's closures only, of every closure type. For per-release channels, the seed comes from the previous release's closure assets. For cumulative-only channels, it is filtered out of the cumulative bundles. With the seed, only narinfos that are new are fetched.

**Backfill.** Targets added later, such as the helper closure types, can be indexed for releases that are already recorded: set the workflow's `backfill_targets` input (or `NFI_BACKFILL_TARGETS`) to e.g. `helpers:direct,helpers:full`. Instead of indexing new releases, the run indexes those targets for recorded releases that lack any of their outputs, uploads their per-release assets, and folds them into the cumulative bundles. A release's notes are rewritten to cover all of its outputs, including those published before.

**Recovery.** If a run fails after publishing some per-release bundles but before updating `releases.txt`, the next run reuses those bundles instead of indexing them again. Merging a release that is already in a cumulative bundle is a no-op.

**Using the bundles.** The workflow doesn't ingest anything. To load published bundles into a query service:

```sh
D=https://github.com/techninja1008/nix-fetch-indexer-data/releases/download
# Catch up on a whole channel:
nfi-admin ingest $D/nixos-26.05/26.05-store-paths-cumulative.nfib $D/nixos-26.05/26.05-helpers-direct-cumulative.nfib \
  $D/nixos-26.05/26.05-packages-cumulative.nfib --wait
# Then add releases one at a time:
nfi-admin ingest $D/nixos-26.05.889.b51242d7d436/nixos-26.05.889.b51242d7d436-store-paths.nfib --wait
```

Bundles published before closure types (format version 2) are read as `store-paths` closures.

The script can also run locally. Set `NFI_STORE=dir:<path>` to write to a directory instead of GitHub, and `NFI_INDEX_ARGS="--sample 30 --probe-systems x86_64-linux"` for a quick test.

## Interchange format

A bundle holds a table of releases, a table of evaluations, and up to five sections: `closure`, `packages`, `outpaths`, `programs` and `eval-errors`.

- **Release membership:** each entry has a bitset over the release table. One file can therefore hold many releases and still be split or filtered per release.
- **Evaluations and closures (format version 3):** the evaluation table lists (release, eval type) pairs. The closure section has its own table of closures, each an evaluation plus a closure type name; closure path and missing-path bitsets index that table. The eval-errors section has a bitset of the evaluations whose errors it holds, and each error a bitset of the evaluations it occurred in. Version 2 bundles have no closure table and their bits are release indices; they are upgraded on read, with one evaluation and closure per release of a type chosen by the reader (`--v2-closure-type`, default `store-paths`).
- **Binary form:** `NFIB`, a container version byte (currently 2), a compression byte, then compressed CBOR. In this form entries are positional arrays, store hashes are 20 raw bytes and bitsets are byte strings.
- **JSON form:** the same data model, with nix-base32 hashes and hex bitsets. The JSON form is detected by its leading `{`.

### Compression benchmark

These figures come from `bench-compression` on a full `nixos-25.05` closure bundle (320,419 paths, 6.1M reference edges), run with a release build on 4 cores.

| algorithm | bytes | encode | decode |
|---|---:|---:|---:|
| none | 66,100,519 | 0.18 s | 0.54 s |
| zstd:3 | 29,035,513 | 0.25 s | 0.47 s |
| **zstd:9** (default) | 26,022,350 | 0.47 s | 0.59 s |
| zstd:15 | 24,808,926 | 3.7 s | 0.65 s |
| zstd:19 | 21,650,286 | 14.7 s | 1.05 s |
| xz:6 | 22,709,338 | 30.8 s | 2.38 s |
| brotli:9 | 23,550,686 | 32.7 s | 1.14 s |

**Default: zstd:9**

- It encodes about 30× faster than zstd:19: about 0.5 s for a release, and seconds rather than minutes for a whole channel.
- It decodes quickly, and decoding happens on every ingest.
- Its files are about 20% larger than zstd:19, which gives the smallest files of the algorithms tested. Use `--compression zstd:19` when bundle size matters more than write time.
- Multithreaded zstd uses 4 MiB jobs, so all cores are busy, and long-distance matching is on.

**Layout:** in the binary form, entries are written as positional CBOR arrays rather than maps with field names, which keeps the encoded closure at 66 MB rather than 84 MB. The JSON form keeps named fields.

**Indexing time:** a full `nixos-25.05` closure (320k narinfos) indexes and writes in about 40 s with the defaults:

- about 35 s fetching at roughly 9–10k narinfos/s
- under 1 s writing at zstd:9 (about 15 s at zstd:19)

## Query service

```sh
nfi-service \
  --database-url postgres://nfi@localhost/nfi \
  --meili-url http://localhost:7700 \
  --listen 0.0.0.0:8080 --admin-listen 127.0.0.1:8081   # optional: NFI_ADMIN_TOKEN=...
```

**Configuration**

All options can also be set through the environment:

- `NFI_DATABASE_URL`
- `NFI_MEILI_URL`, `NFI_MEILI_KEY`, `NFI_MEILI_INDEX`
- `NFI_LISTEN`, `NFI_ADMIN_LISTEN`, `NFI_ADMIN_TOKEN`
- `NFI_NO_WORKER`

**Container image:** `nfi-service-image` only listens on port 8080 from outside the container. The admin listener is bound to `127.0.0.1:8081` inside the container, so it can't be reached by default. To run admin commands from the host, set a token, listen on all interfaces inside the container, and publish the port on a loopback or private address only:

```sh
docker run -p 8080:8080 -p 127.0.0.1:8081:8081 \
  -e NFI_ADMIN_LISTEN=0.0.0.0:8081 -e NFI_ADMIN_TOKEN=... \
  -e NFI_DATABASE_URL=... -e NFI_MEILI_URL=... nfi-service:latest
```

Every replica serves queries. Every replica also runs a job worker, unless `--no-worker` is set.

**Jobs and scaling**

- Jobs (ingest, prune, reindex) live in a Postgres table. Any replica can claim them with `SKIP LOCKED`, and a stale heartbeat makes a job reclaimable.
- Uploads are staged in Postgres, so it does not matter which replica receives them.
- Writers are serialised with an advisory lock.

### Single-node mode (embedded PostgreSQL and Meilisearch)

For a simple single node, the service can run its own PostgreSQL and Meilisearch as supervised child processes. This is implemented in the `nfi-embedded-postgres` and `nfi-embedded-meilisearch` crates.

```sh
nfi-service --embedded-postgres /var/lib/nfi/pg --embedded-meilisearch /var/lib/nfi/meili
```

**PostgreSQL**

- The cluster lives in `<dir>/pgdata`. It is created with `initdb` on first start.
- The server listens only on a Unix socket in `<dir>`; there is no TCP listener.
- PostgreSQL refuses to run as root, so the service must run as an unprivileged user.
- Binaries are found from `--pg-bindir`, then `pg_config`, then the `PATH`. The Nix package puts `postgresql` on its `PATH`.
- A cluster created by another PostgreSQL major version is refused. Upgrade it with `pg_upgrade`, or delete it and re-ingest the bundles.
- A second service on the same directory is refused.

**Meilisearch**

- The database lives in `<dir>/data.ms`.
- The server listens on a random loopback port, with a random master key generated on each start.
- Search data is derived from PostgreSQL. When the database is new, or was written by a different Meilisearch version, it is discarded and a `reindex` job is queued automatically.

**Shutdown**

- On SIGINT or SIGTERM, the service stops serving, then stops Meilisearch, then PostgreSQL.
- On Linux the children are also signalled if the service dies.

**Combining modes**

- Either backend can be used on its own, alongside an external instance of the other (e.g. `--embedded-postgres` with `--meili-url`).
- Embedded PostgreSQL implies a single replica, so `--no-worker` is not allowed with it.
- The cargo features `embedded-postgres` and `embedded-meilisearch` are on by default.

**Ingest performance**

The figures below were measured with embedded PostgreSQL 16 on 4 cores.

| workload | time |
|---|---|
| One full release (320k paths, 6.1M references) into an empty database | ~18 s |
| The same release again (nothing new) | ~7.5 s |
| All 87 `nixos-26.05` releases in one bundle (2.7M paths, 22.5M memberships, 38M references) | ~3.2 min |
| An extra release into that database (315k new paths) | ~24 s |
| Pruning one release, or a whole channel, from it | ~20 s |

### Public API

| method | path | description |
|---|---|---|
| GET | `/v1/search?q=&attr=&pname=&version=&program=&system=&release=&channel=&series=&commit=&limit=&offset=` | Search over packages and programs |
| GET | `/v1/packages/{attr}?commit=&system=` | Metadata, output paths (with a `cached` flag) and programs for an attribute at a commit; unique commit prefixes are accepted |
| POST | `/v1/lookup/bulk` | Body `{"items":[{"attr","commit","system"?,"outputs"?}]}`; returns per-item status and outputs, plus the deduplicated closure |
| POST | `/v1/minimise?close_miss_threshold=` | Body: raw `nix derivation show --recursive` JSON (legacy or v4 format) or a reduced graph `{"derivations":{drv:{outputs,inputDrvs,inputSrcs,fixed}}}` |

**Bulk lookup details**

- Output selection: the default is `out`, or all outputs when there is no `out`. `["*"]` selects all outputs.
- Item statuses: `ok`, `unknown_commit`, `ambiguous_commit`, `unknown_attr`, `no_outpaths`, `not_cached`.

**Minimise algorithm**

- The requested derivations are those that no other derivation in the graph references.
- The graph is walked from the requested derivations, dependants first.
- A derivation is a cache hit when every output its dependants need has a path in the index, in a closure of any type. The walk does not descend past a hit.
- Any other derivation must be built (or, for a fixed-output derivation, fetched), and the walk continues into its inputs.
- Fixed-output paths are computed from their hash when `nix derivation show` leaves them out (Nix 2.34+).

**Minimise response**

- `to_build`: the derivations that must be built.
- `to_fetch`: the fixed-output derivations (sources and other fetches) that aren't cached and will be downloaded from upstream.
- `roots`: hits that are not inside another hit's closure.
- `closure`: the full runtime closure of the hits.
- `unresolved`: input derivations missing from the graph.
- `warnings`: problems for the client to show. Each has a `code`, a `message` and the `paths` it concerns.
  - `unindexed_nixpkgs` is raised for every final `stdenv-linux`/`stdenv-darwin` derivation that must be built. A stdenv beneath a cached path doesn't count. Graphs can mix several nixpkgs instances, and each unindexed one is reported. Hydra builds stdenv for every release, so this almost always means the graph was evaluated from a nixpkgs revision that isn't indexed, or with overlays/config that change stdenv.
  - `close_miss` lists derivations to build whose name (name and version) matches indexed paths with a different hash: probably a slightly overridden package, or a build input that isn't indexed. Names matching `close_miss_threshold` indexed paths or more (default 100) are too generic to report; 0 turns the check off (`nfi minimise --close-miss-threshold`).
  - `nfi minimise` prints warnings to stderr.

### Admin API (admin listener only, optional `Authorization: Bearer <token>`)

| method | path | description |
|---|---|---|
| POST | `/admin/v1/ingest?v2_closure_type=` | Streamed bundle body (binary or JSON), or `{"url": ..., "v2_closure_type": ...}` with `Content-Type: application/json`; returns `{"id"}`. `v2_closure_type` (default `store-paths`) is the closure type to read a version 2 bundle as (`nfi-admin ingest --v2-closure-type`) |
| GET | `/admin/v1/jobs`, `/admin/v1/jobs/{id}` | Job status, progress and result |
| GET | `/admin/v1/releases` | Releases (name, commit, channel, series) with their datasets and evaluations: eval type, closure type names, and errors (`<attr>@<system>`, or `null` when not indexed) |
| POST | `/admin/v1/prune` | `{"releases":[], "commits":[], "channels":[], "closure_types":[]}`; garbage-collects orphaned paths and packages, then rebuilds search. With `closure_types` (`nfi-admin prune --closure-type`), only the matching closures of the selected releases are removed (`helpers` or `helpers:full`, say) and the releases are kept; evaluations left with neither closures nor indexed errors are removed too |
| POST | `/admin/v1/reindex` | Rebuilds the Meilisearch index from Postgres and swaps it in |
| POST | `/admin/v1/check` | Checks closure-index consistency (see below); the job result is a report with `ok`, counts and example rows per issue |

Ingest is purely additive: entries are immutable, so every insert is `ON CONFLICT DO NOTHING` and re-ingesting (or ingesting overlapping bundles) is a no-op for data already present. Nothing is deleted on ingest; rows are only removed by `prune`, which also garbage-collects orphaned store paths and packages. A bundle whose release name is already indexed with a different commit is rejected.

### Consistency

Closure membership and references are arrays on store path rows, so PostgreSQL can't enforce them with foreign keys. Ingest and prune maintain the invariants instead, and a few safeguards back them up.

**`nfi-admin check`** runs these checks in one read-only snapshot:

| issue | meaning |
|---|---|
| `dangling_refs` | a reference to a store path id that doesn't exist |
| `unknown_closures` | membership in a closure that doesn't exist |
| `orphan_paths` | a store path that belongs to no closure |
| `open_closures` | a path is in a closure that one of its references isn't in |

The check takes about 20 s on all of `nixos-26.05`: 2.7M paths and 38M references.

**Queries fail instead of returning partial results.** If a closure query reaches a dangling reference or a path in no closure, the request fails with HTTP 500 and an `index inconsistency: …` message. It doesn't return a smaller closure.

**Bundles are validated.** `Bundle::validate`, which runs in the indexers and at ingest, rejects closures that aren't closed, and closures of releases without the closure dataset. Ingest doesn't store paths that belong to no closure.

## CLIs

```sh
export NFI_URL=http://nfi.example:8080
nfi search ripgrep --series 25.05
nfi search --program rg
nfi show ripgrep --commit ac62194c3917
nfi bulk ripgrep@ac62194c3917 jq@ac62194c3917 --outputs out,man
nfi minimise .#mypackage --fetch-hook 'sh -c "xargs nix-store --realise < {}"'
nix derivation show --recursive .#foo | nfi minimise -f -
```

**Fetch hook**

- Configuration: `--fetch-hook`, `$NFI_FETCH_HOOK`, or `fetch_hook` in `~/.config/nfi/config.toml`. The config file can also set `url`, `system` and `nix`.
- Invocation: the CLI writes the resolved paths, one per line, to a temporary file, then runs the hook synchronously in the CLI's terminal.
  - `{}` in the command is replaced by the file's path. If there is no `{}`, the path is appended.
  - `minimise` passes the closure by default, or only the roots with `--roots-only`.
- Exit status: the CLI exits with the hook's exit status.

**Admin CLI**

```sh
export NFI_ADMIN_URL=http://127.0.0.1:8081 NFI_ADMIN_TOKEN=...
nfi-admin ingest release.nfib https://example/other.nfib --wait
nfi-admin releases
nfi-admin prune --channel nixos-24.11 --wait
nfi-admin prune --commit ac62194c3917 --release nixos-25.05.813814.ac62194c3917
nfi-admin reindex --wait
nfi-admin check --wait      # exit status 3 if the index is inconsistent
nfi-admin jobs [ID] [--wait]
```

## Tests

```sh
cargo test --workspace                       # unit and integration tests
NFI_TEST_REQUIRE_SERVICES=1 cargo test ...   # fail instead of skipping when Postgres/Meilisearch are missing
NFI_LIVE_TESTS=1 cargo test -p nfi-indexer live_smoke   # index a sample of a real release over the network
NFI_LIVE_TESTS=1 cargo test -p nfi --test live           # live index -> service -> CLI round trip
NFI_LIVE_TESTS=1 NFI_LIVE_BUNDLE=full.nfib cargo test --release -p nfi --test live   # plus real-nix minimise
```

### Live end-to-end run (full nixos-25.05 closure, packages and programs)

| step | time |
|---|---|
| Ingest: 320k paths, 6.1M edges, 129k packages, 153k programs, plus 153k search documents | ~145 s |
| `minimise` on the release's own `hello` (all cached: 1 root, 5-path closure) | ~1 s, most of it spent in `nix derivation show` |
| `minimise` on a modified `hello`: 2 derivations to build, 8 roots, 82-path closure | ~0.3–2.7 s |

### Acceptance test: minimise this repository

`nfi minimise .#nfi`, run against the exact nixpkgs release in `flake.lock` (`nixos-26.11pre1092090.55bfb4d7437b`), should only plan derivations that cache.nixos.org doesn't have. Indexed with `--targets store-paths,helpers:direct,helpers:full`:

| | store-paths only | with helper closures |
|---|---:|---:|
| to build | 762 | 300 |
| to fetch | (in to build) | 297 |
| cached but still planned | 541 | 376 |
| `unindexed_nixpkgs` warning | yes | no |

- **Indexing:** 56 s for 264,841 paths. All 20 probes ran on both systems.
- **No longer planned:** the bootstrap curl and the other helper tools, and every stdenv bootstrap stage.
- **Still planned although cached:** all 376 are crate builds and crate tarballs from this repository's `Cargo.lock` that nixpkgs happens to share. Indexing those needs a closure type built from full evaluation (see Known limitations).

## Known limitations

- **Most fixed-output sources are not in the index.** `store-paths.xz` lists only the outputs of Hydra jobs, and the closure indexer follows their *runtime* references. Only the sources in helper probes' build graphs are indexed (`helpers:full`). Other source tarballs, such as crates from a project's `Cargo.lock`, are cached on cache.nixos.org when nixpkgs uses them but aren't indexed; `minimise` reports them in `to_fetch`. So are build-only derivations that nixpkgs happens to share with a project (e.g. its crate builds). Indexing these needs a future closure type built from full evaluation.
- **Out paths need evaluation.** `packages.json` contains no store paths. Lookups only return outputs for releases whose `outpaths` dataset has been indexed (with nix-eval-jobs); other items get status `no_outpaths`.

**Test services**

- The integration tests start Postgres from `pg_config --bindir`, `/usr/lib/postgresql/*/bin` or `NFI_TEST_PG_BINDIR`.
- They start Meilisearch from `PATH` or `NFI_TEST_MEILI_BIN`.
- To use existing servers instead, set `NFI_TEST_DATABASE_URL` and `NFI_TEST_MEILI_URL`.

## License

MIT. See [LICENSE](LICENSE).
