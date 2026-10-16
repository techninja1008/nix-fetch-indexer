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

nfi-indexer closure  --release-url $R -o closure.nfib    # store-paths.xz + narinfos
nfi-indexer packages --release-url $R -o packages.nfib   # packages.json.br
nfi-indexer programs --release-url $R -o programs.nfib   # programs.sqlite (nixos channels only)
nfi-indexer outpaths --release-url $R --systems x86_64-linux,aarch64-linux -o outpaths.nfib  # nix-eval-jobs
nfi-indexer all      --release-url $R --datasets closure,packages,programs,outpaths -o release.nfib

# Index every release of a channel, or only the newest few:
nfi-indexer all --channel-list nixos/25.05 --limit 3 -o recent.nfib

# Incremental: reuse a previous bundle's closure so known narinfos are not fetched again.
nfi-indexer closure --release-url $R2 --seed closure.nfib -o closure-2.nfib
```

**Indexer options**

- `--concurrency`: number of concurrent narinfo requests (default 64).
- `--cache-url`: the binary cache to query (default `https://cache.nixos.org`).
- `--sample N`: index only the first N roots, for testing.
- `--format json`: write the JSON form.
- `--compression {none,zstd,xz,brotli}[:level]`: compression for the binary form (default `zstd:19`).

**Bundle tools**

```sh
nfi-indexer merge a.nfib b.nfib -o ab.nfib
nfi-indexer split ab.nfib -o per-release/
nfi-indexer filter ab.nfib --channel nixos-25.05 --section closure,packages -o f.nfib
nfi-indexer convert ab.nfib --format json -o ab.json
nfi-indexer inspect ab.nfib
nfi-indexer bench-compression ab.nfib
```

## Interchange format

A bundle holds a table of releases plus up to four sections: `closure`, `packages`, `outpaths` and `programs`.

- **Release membership:** each entry has a bitset over the release table. One file can therefore hold many releases and still be split or filtered per release.
- **Binary form:** `NFIB`, a container version byte, a compression byte, then compressed CBOR. In this form store hashes are 20 raw bytes and bitsets are byte strings.
- **JSON form:** the same data model, with nix-base32 hashes and hex bitsets. The JSON form is detected by its leading `{`.

### Compression benchmark

These figures come from `bench-compression` on a real `nixos-25.05` bundle, run with a release build on 4 cores.

The bundle contains:

- a 20,803-path closure sample
- 129,498 packages
- 152,764 programs

| algorithm | bytes | encode | decode |
|---|---:|---:|---:|
| none | 47,489,350 | 0.07 s | 0.84 s |
| zstd:3 | 8,489,081 | 0.35 s | 0.54 s |
| zstd:9 | 7,243,393 | 0.58 s | 0.59 s |
| **zstd:19** (default) | 6,121,250 | 29.7 s | 0.90 s |
| xz:6 | 5,964,526 | 20.9 s | 0.95 s |
| xz:9 | 5,884,070 | 25.0 s | 1.10 s |
| brotli:9 | 6,620,546 | 5.8 s | 0.53 s |
| brotli:11 | 5,835,451 | 95.8 s | 0.48 s |

The same bundle is 56.8 MB as JSON.

**Default: zstd:19**

- It decodes quickly, and decoding is done on every ingest.
- Its output is within 5% of the smallest results (xz and brotli:11).

For quick iteration, `--compression zstd:3` encodes about 85× faster but produces a file about 40% larger.

A full `nixos-25.05` closure, from a live run, is:

- 320,419 paths and 6.1M reference edges
- 30 MB at zstd:3
- indexed in about 6 minutes at a concurrency of 96

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

Every replica serves queries. Every replica also runs a job worker, unless `--no-worker` is set.

**Jobs and scaling**

- Jobs (ingest, prune, reindex) live in a Postgres table. Any replica can claim them with `SKIP LOCKED`, and a stale heartbeat makes a job reclaimable.
- Uploads are staged in Postgres, so it does not matter which replica receives them.
- Writers are serialised with an advisory lock.

### Public API

| method | path | description |
|---|---|---|
| GET | `/v1/search?q=&attr=&pname=&version=&program=&system=&release=&channel=&series=&commit=&limit=&offset=` | Search over packages and programs |
| GET | `/v1/packages/{attr}?commit=&system=` | Metadata, output paths (with a `cached` flag) and programs for an attribute at a commit; unique commit prefixes are accepted |
| POST | `/v1/lookup/bulk` | Body `{"items":[{"attr","commit","system"?,"outputs"?}]}`; returns per-item status and outputs, plus the deduplicated closure |
| POST | `/v1/minimise` | Body: raw `nix derivation show --recursive` JSON (legacy or v4 format) or a reduced graph `{"derivations":{drv:{outputs,inputDrvs,inputSrcs}}}` |

**Bulk lookup details**

- Output selection: the default is `out`, or all outputs when there is no `out`. `["*"]` selects all outputs.
- Item statuses: `ok`, `unknown_commit`, `ambiguous_commit`, `unknown_attr`, `no_outpaths`, `not_cached`.

**Minimise algorithm**

- The requested derivations are those that no other derivation in the graph references.
- The graph is walked from the requested derivations, dependants first.
- A derivation is a cache hit when every output its dependants need has a path in the index. The walk does not descend past a hit.
- Any other derivation must be built, and the walk continues into its inputs.

**Minimise response**

- `to_build`: the derivations that must be built.
- `roots`: hits that are not inside another hit's closure.
- `closure`: the full runtime closure of the hits.
- `unresolved`: input derivations missing from the graph.

### Admin API (admin listener only, optional `Authorization: Bearer <token>`)

| method | path | description |
|---|---|---|
| POST | `/admin/v1/ingest` | Streamed bundle body (binary or JSON), or `{"url": ...}` with `Content-Type: application/json`; returns `{"id"}` |
| GET | `/admin/v1/jobs`, `/admin/v1/jobs/{id}` | Job status, progress and result |
| GET | `/admin/v1/releases` | Releases (name, commit, channel, series) with their datasets |
| POST | `/admin/v1/prune` | `{"releases":[], "commits":[], "channels":[]}`; garbage-collects orphaned paths and packages, then rebuilds search |
| POST | `/admin/v1/reindex` | Rebuilds the Meilisearch index from Postgres and swaps it in |

Re-ingesting a release replaces that release's rows for every dataset the bundle carries, so ingestion is idempotent.

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
nfi-admin jobs [ID] [--wait]
```

## Tests

```sh
cargo test --workspace                       # unit and integration tests
NFI_TEST_REQUIRE_SERVICES=1 cargo test ...   # fail instead of skipping when Postgres/Meilisearch are missing
NFI_LIVE_TESTS=1 cargo test -p nfi-indexer live_smoke   # index a sample of a real release over the network
```

**Test services**

- The integration tests start Postgres from `pg_config --bindir`, `/usr/lib/postgresql/*/bin` or `NFI_TEST_PG_BINDIR`.
- They start Meilisearch from `PATH` or `NFI_TEST_MEILI_BIN`.
- To use existing servers instead, set `NFI_TEST_DATABASE_URL` and `NFI_TEST_MEILI_URL`.
