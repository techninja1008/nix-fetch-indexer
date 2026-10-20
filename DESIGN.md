# nix-fetch-indexer — design

Tooling, written in Rust, for indexing the binary-cache closure of NixOS / nixpkgs releases and querying it.
The main goal is to find the minimal set of cached store paths that is useful for building a given derivation.

## Workspace layout

A single Cargo workspace:

| crate | kind | purpose |
|---|---|---|
| `nfi-interchange` | lib | Interchange bundle data model, CBOR + compression codec, JSON codec, merge/split/filter |
| `nfi-indexer` | bin | Indexers (`index --targets`: closure types `store-paths`, `helpers:direct` and `helpers:full`, `packages`, `outpaths`, `programs`), plus offline bundle tooling (`merge`, `split`, `filter`, `convert`, `inspect`) |
| `nfi-service` | bin | Query service (axum HTTP/JSON, Postgres, Meilisearch) |
| `nfi` | bin | Query CLI (non-administrative endpoints, fetch hook, flake-ref resolution) |
| `nfi-admin` | bin | Admin CLI (ingest, list releases, prune, reindex, job status) |
| `nfi-embedded-postgres` | lib | Supervised child-process PostgreSQL for single-node mode (`--embedded-postgres`) |
| `nfi-embedded-meilisearch` | lib | Supervised child-process Meilisearch for single-node mode (`--embedded-meilisearch`) |
| `nfi-api` | lib | Shared request/response types for the service and CLIs |

Packaging uses Nix flake packages for all binaries, plus a dev shell.
OCI images for the service are built with Nix `dockerTools`. There is no Dockerfile.

## Sources (facts)

- **Release directory:** a release lives at `https://releases.nixos.org/<project>/<series>/<release-name>`, for example `nixos/25.05/nixos-25.05.813814.ac62194c3917`.
  - The bucket is `nix-releases` on S3.
  - You can list it with `https://nix-releases.s3.amazonaws.com/?delimiter=/&prefix=nixos/25.05/`.
- **`git-revision`:** the full 40-char commit, with no trailing newline.
- **`store-paths.xz`:** one `/nix/store/<hash>-<name>` per line.
  - Contains output paths only, for all outputs and all Hydra platforms.
- **`packages.json.br`:** `{"version":"2","packages":{<attrPath>:{name,pname,version,system,outputName,outputs,meta}}}`.
  - It has **no store paths** (every `outputs` value is `null`).
- **`nixexprs.tar.xz`:** contains `<dir>/programs.sqlite` on nixos channels only.
  - Table: `Programs(name, system, package)`.
- **narinfo:** fetched from `https://cache.nixos.org/<hash>.narinfo`.
  - `References` and `Deriver` are basenames.
  - The cache supports HTTP/2.
  - A missing path returns 404.

## Identity

- A **release** is keyed by its **release name**, for example `nixos-25.05.813814.ac62194c3917` or `nixpkgs-26.11pre123.abcdef`.
- Each release has these attributes:
  - `commit`: full hash, assumed unique to the release
  - `channel`: e.g. `nixos-25.05`, `nixos-unstable`, `nixpkgs-unstable`
  - `series`: e.g. `25.05` or `unstable`
  - `url`
  - `indexed_at`
- **Commit lookups:** lookups by commit resolve to the release with that commit. Unique hash prefixes are accepted.
- **Store path keys:** store paths are keyed by their 32-char nix-base32 hash. The hash is unique across systems, so the closure graph is system-agnostic.
- **System tags:** package, out-path and program entries are tagged with `system`.

## Interchange format

A **bundle** holds multiple sections that share one release table:

```
Bundle {
  format_version: u32,
  releases: [Release],              // release table; bit i of every release bitset refers to releases[i]
  evals:    [Eval { release: release index, eval_type }],   // evaluations: closures and eval errors refer to these
  closure:  Option<ClosureSection>,
  packages: Option<PackagesSection>,
  outpaths: Option<OutpathsSection>,
  programs: Option<ProgramsSection>,
  eval_errors: Option<EvalErrorsSection>,
}
Release { name, commit, channel, series, url, datasets: [closure|packages|outpaths|programs|eval-errors] }

ClosureSection {
  closures: [Closure { eval: eval index, closure_type }],   // closure type name within the eval type; may be empty
  paths: [ClosurePath { hash, name, references: [path index], nar_size, file_size, deriver: Option<String>, closures: Bitset }],
  missing: [MissingPath { hash, name, closures: Bitset }]   // roots that are 404 on the cache
}
PackagesSection { entries: [Package { attr, name, pname, version, system, description, homepage, license, main_program, broken, unfree, releases: Bitset }] }
OutpathsSection { entries: [Outpaths { attr, system, drv_path, outputs: {name: store path}, releases: Bitset }] }
ProgramsSection { entries: [Program { name, system, attr, releases: Bitset }] }
EvalErrorsSection {
  evals: Bitset,                                           // evaluations whose errors the section holds
  errors: [EvalError { attr, system, evals: Bitset }]      // attributes that failed to evaluate
}
```

- `datasets` on a release records which sections contain data for it.
  - A release can be present in a section and still have zero entries.
- **Evaluations and closures (format version 3).** An **evaluation** is a release evaluated in one way, named by its **eval type** (the evaluator): `store-paths` (a pseudo-evaluation: the roots come from Hydra's evaluation via `store-paths.xz`), `helpers` (helper probes), and later possibly a full nixpkgs evaluation or `fods`.
  - Each evaluation produces closures, one per **closure type** of its eval type. Closure type names are namespaced by eval type, may be empty, and are unique within it: `store-paths` has one unnamed closure type, `helpers` has `direct` and `full`. They are written `<eval type>[:<name>]`, e.g. `store-paths`, `helpers:full`.
  - A closure refers to its evaluation, not to a release: it inherits the evaluation's release. Every bundle with closures has the evaluations they refer to.
  - Closure path and missing-path bitsets index the closure table; eval error bitsets index the evaluation table; other sections' bitsets index releases.
  - A closure's release must carry the closure dataset.
- **Eval errors.** Eval errors are to evaluations what missing paths are to closures.
  - An error is keyed by (attr, system). Evaluation is deterministic, so no message is kept.
  - For each evaluation the section is either present (its `evals` bit is set: these are all its errors, possibly none) or absent (nothing is known about its errors). Errors may only be in covered evaluations, whose releases must carry the eval-errors dataset.
  - Eval errors only come from actually evaluating something: an evaluator writes them with its closures. The `store-paths` pseudo-evaluation records none.
  - Out paths are partly the evaluation counterpart of closure paths; folding them into evaluations later would only change the out paths' own details.
- **Version 2 bundles** have no closure or evaluation table; their closure bits are release indices. Readers upgrade them, with one evaluation and closure per release with the closure dataset, of a closure type the caller chooses (`--v2-closure-type`, e.g. `helpers:full`; default `store-paths`), so no bitset is rewritten.
- **References** are indices into `paths`, which keeps the binary form compact. Paths outside the indexed set do not exist, because the closure is followed transitively.
- **Bitsets** are encoded as little-endian byte strings.
- **Binary form:**
  - Entries (closure paths, missing paths, packages, outpaths, programs) are positional CBOR arrays instead of maps, keeping field names out of every entry. The JSON form uses named fields.
  - Layout: magic `NFIB`, a one-byte container version (2), a one-byte compression id, then the compressed CBOR body.
  - Compression ids: `0` none, `1` zstd, `2` xz, `3` brotli.
  - Benchmark (`nfi-indexer bench-compression`): per algorithm, compressed size and encode/decode time.
  - Default: **zstd level 9** for indexer output (fast to write, quick to decode); `zstd:19` gives about 17% smaller files for slower writes.
- **JSON form:** the same data model.
  - Structurally identical, with bitsets as hex strings and references as index arrays.
  - It is detected by a leading `{`.
- **Merge:** union the release tables by name, the evaluation tables by (release, eval type) (sorted by release, then eval type), and the closure tables by (evaluation, closure type), remap the bitsets, and union entries by key:
  - closure: hash
  - packages: (attr, version, system, metadata)
  - outpaths: (attr, system, drv_path)
  - programs: (name, system, attr)
  - eval errors: (attr, system); covered evaluations are unioned
- **Split / filter:** select a subset of releases, sections, eval types (evaluations with their closures and eval errors) and/or closure types (exact, e.g. `helpers:full`), then drop the entries whose bitset becomes empty, and evaluations left with neither closures nor covered errors. A release left without closures loses the closure dataset.

## Indexers (`nfi-indexer`)

- **Command:** `nfi-indexer index` runs the indexers for `--targets` (default `store-paths,packages,programs`). Targets are `packages`, `programs`, `outpaths`, and closure types (`store-paths`, `helpers:direct`, `helpers:full`); an evaluator alone isn't a target. Eval errors aren't a target: an evaluator records them with its closures.
- **Input:** `--release-url <full URL>` or `--channel-list <prefix>`. The second option enumerates every release under a prefix via the S3 listing API, with optional `--limit` and `--since`.
- **Output:** a bundle written with `-o <file>` and `--format binary|json`.
- **Closures** (`store-paths`)
  - Fetch `store-paths.xz`, then mass-query narinfos over HTTP/2 with bounded concurrency (default 512), spread over several independent connections (`--connections`, default 8), because the cache caps concurrent streams per connection.
  - Retries back off on 429/5xx.
  - The indexer follows references transitively.
  - Paths that return 404 are recorded in `missing`.
  - `--seed <bundle>` reuses known hashes: store paths are immutable, so only new hashes are fetched. In channel-list mode, each release seeds the next.
  - Every closure type goes through the same walk from its roots; only root collection differs.
- **Helper probes** (the `helpers` evaluator: `helpers:direct`, `helpers:full`)
  - nixpkgs helpers build some tools inline, e.g. fetchurl's own bootstrap curl. Those tools, and the stdenv bootstrap, are in no runtime closure of `store-paths.xz`, even though Hydra built and cached them.
  - `probes.nix` (embedded in the indexer) defines probes against the release's unpacked nixexprs, for each `--probe-systems` system (default `x86_64-linux,aarch64-linux`):
    - **fetch probes** call a helper twice with every argument changed. Only derivations shared by both calls are kept, so anything built from the probe's own arguments drops out. Option variants (e.g. fetchgit with submodules or LFS) are separate probes, unioned.
    - **target probes** are roots themselves (stdenv, stdenvNoCC).
  - Probes guard on helper and argument availability per nixpkgs version; a probe that doesn't apply is left out of `probes'`. Each remaining probe is instantiated separately, so one that fails to evaluate (`nix-instantiate` exits non-zero) doesn't stop the others. It is recorded as an eval error of the release's `helpers` evaluation. Failures to run the tools at all, or unexpected output, fail the run.
  - Probes run once per release for whichever `helpers` closure types are selected; both closures belong to the one evaluation, which also covers its eval errors.
  - `nix derivation show --recursive` gives the probes' graphs. `direct` roots are the consumed outputs of shared direct inputs (and target outputs); `full` roots are all outputs of every shared derivation in the build graphs, including bootstrap stages and fixed-output sources.
- **`packages`**
  - Fetch `packages.json.br`, stream-decode brotli, and emit package metadata.
- **`outpaths`**
  - Download and extract `nixexprs.tar.xz`, then run `nix-eval-jobs` for each system in `--systems` (default `x86_64-linux`).
  - Emit attr → drvPath → output → path.
  - `nix-eval-jobs` must be on `PATH`.
- **`programs`**
  - Download `nixexprs.tar.xz` and extract `programs.sqlite`.
  - Emit an empty section, with the release marked as having the dataset, when the tarball does not ship the file.
- **Offline tools:** `merge`, `split`/`filter`, `convert`, `inspect`/`stats`, `bench-compression`.

## Query service (`nfi-service`)

- **Store:** Postgres is the source of truth. Caching may be added later.
- **Search:** Meilisearch.
- **Listeners:** a public listener for queries, and a separate admin listener. The admin listener accepts an optional static bearer token (`NFI_ADMIN_TOKEN`).

### Single-node mode

`--embedded-postgres <dir>` and `--embedded-meilisearch <dir>` run private servers as child processes of the service, for running a single node without external infrastructure.

**PostgreSQL**

- Listens on a Unix socket only.
- Must not run as root.
- Refuses a cluster created by another major version.
- Refuses to start if another server already holds the data directory.

**Meilisearch**

- Listens on loopback only, with a random master key.
- Its data is derived from PostgreSQL. A new database, or one written by another version, is discarded and rebuilt through a `reindex` job.

**Lifecycle**

- Children are started from the main thread and stopped on shutdown.
- On Linux, `PR_SET_PDEATHSIG` also signals them if the service dies.

### Postgres schema (sketch)

```
releases(id, name unique, commit unique, channel, series, url, indexed_at)
release_datasets(release_id, dataset)
eval_types(id, name unique)             -- store-paths, helpers, ...
evals(id, release_id, type_id, errors_indexed, unique (release_id, type_id))
closure_types(id, eval_type_id, name, unique (eval_type_id, name))   -- (store-paths, ''), (helpers, direct), (helpers, full)
closures(id, eval_id, type_id, unique (eval_id, type_id))            -- type's eval type = evaluation's
store_paths(id bigserial, hash char(32) unique, name, nar_size, file_size, deriver,
            closures int[],   -- sorted ids of the closures containing the path
            refs bigint[])    -- ids of the path's runtime references
missing_paths(hash, name, closure_id)
eval_errors(attr, system, eval_id)      -- one row per (error, evaluation), like missing_paths; only in evals with errors_indexed
packages(id, attr, version, system, name, pname, description, homepage, license, main_program, broken, unfree)
package_releases(package_id, release_id)
package_outputs(attr, system, release_id, output, path_hash, drv_path)   -- joined from outpaths at ingest
programs(name, system, attr, release_id)
jobs(id, kind, status, params jsonb, staged_blob oid/bytea, progress, error, created_at, started_at, finished_at)
```

Membership and references are arrays on the path row, not one row per (path, closure) or per edge. Migration 0003 introduced evaluations and closures by giving each existing release a store-paths evaluation and closure with the release's own id, so existing arrays stayed valid. A channel's releases share most of their paths, so per-pair rows grow quickly: all 87 `nixos-26.05` releases are 2.7M paths, 22.5M memberships and 38M references.

**Ingest of a closure section**
1. Load the bundle's paths into a temporary table.
2. Resolve existing paths by hash. Allocate ids for new paths from the sequence; this is safe because writers hold the advisory lock.
3. Insert the new paths with their references in one statement.
4. Add the bundle's closure ids to the arrays of existing paths. Eval types, evaluations, closure types and closures are created as needed; an evaluation whose errors the bundle covers gets `errors_indexed`.

A store path's references never change, so they are written only once, when the path is new.

**Prune** removes the pruned closures' ids from the arrays, then deletes paths that are left with no closures. It removes whole releases (their evaluations, closures and eval errors cascade), or with `closure_types` only the closures of those closure types of the selected releases; evaluations left with neither closures nor indexed errors are removed too.

**Invariants.** These replace the foreign keys:
- every reference resolves to an existing path;
- every closure id exists;
- every path belongs to at least one closure;
- a path's references belong to all of its closures (closures are closed);
- a closure's type belongs to its evaluation's eval type;
- eval errors are only in evaluations whose errors are indexed.

**Enforcement:**
- `Bundle::validate` rejects open closures.
- Ingest skips paths in no closure.
- `nfi-admin check` (`POST /admin/v1/check`) verifies the invariants.
- Closure queries fail with an `index inconsistency` error instead of returning partial results.

### Jobs

- Admin ingest and prune requests enqueue a job.
- Any replica's worker claims jobs with `SELECT … FOR UPDATE SKIP LOCKED`.
- Uploaded bundles are staged in Postgres before processing.
- Ingest is purely additive (`ON CONFLICT DO NOTHING`): entries are immutable, so re-ingesting is a no-op and nothing is deleted or garbage-collected on ingest. A release name already indexed with a different commit is rejected.
- Prune removes release associations, then garbage-collects store paths, packages and programs that no remaining release references.
- Search documents are updated after each Postgres commit.

### Search documents

- There is one Meilisearch document per `(attr, version, system)`. Fields:
  - `attr`, `pname`, `version`, `system`, `description`
  - `programs[]`, `main_program`
  - `releases[]`, `channels[]`, `series[]`, `commits[]`
- Filterable attributes: `attr`, `pname`, `version`, `system`, `programs`, `releases`, `channels`, `series`, `commits`.
- An admin `reindex` rebuilds the index from Postgres.

### Public API

- `GET /v1/search?q=&attr=&version=&program=&series=&channel=&commit=&system=&limit=&offset=`
- `GET /v1/packages/{attr}?commit=&system=`: direct lookup. Returns metadata plus output store paths, with a cached flag per output.
- `POST /v1/lookup/bulk`
  - Request body: `{items:[{attr, commit, system?}]}`
  - Response:
    - `items`: one per request, with status `ok | unknown_attr | unknown_commit | not_cached` and its paths
    - `closure`: the deduplicated runtime closure of all resolved paths
- `POST /v1/minimise`
  - Request body: either raw `nix derivation show --recursive` JSON or the reduced graph `{derivations:{drvPath:{outputs:{name:path}, inputDrvs:{drv:[outputs]}, inputSrcs:[...]}}}`.
  - Response: `{requested:[drv], roots:[path], closure:[path], to_build:[drv]}`.

### Admin API (admin listener)

- `POST /admin/v1/ingest` is either a streaming body (binary or JSON bundle) or `{url}`. It returns a job id.
- `GET /admin/v1/jobs/{id}` and `GET /admin/v1/jobs` show job status.
- `GET /admin/v1/releases` lists releases with their datasets and evaluations (closure types, and errors when indexed).
- `POST /admin/v1/prune` takes `{releases?:[name], commits?:[hash], channels?:[name]}` and returns a job id.
- `POST /admin/v1/reindex` returns a job id.
- `POST /admin/v1/check` returns a job id. The job's result is a consistency report: `ok`, totals, and per issue kind a count and examples.

### Minimise algorithm

1. Reduce the input to a derivation graph.
2. The **requested** derivations are those that no other derivation in the input references through `inputDrvs`.
3. DFS from the requested derivations.
   - A derivation is a **hit** when every output it needs is a known store path in the index.
     - "Needed" means the outputs its dependants consume, or all outputs for a requested derivation.
     - Paths present in any closure of any type count, and paths in `missing` do not.
   - On a hit, record those paths and stop descending.
   - Otherwise the derivation must be realised: add it to `to_build`, or to `to_fetch` for a fixed-output derivation, and recurse into its `inputDrvs` with the outputs they consume.
     - Fixed-output paths are computed from their hash and name when `nix derivation show` omits them (Nix 2.34+).
     - Floating content-addressed outputs with no known path always count as must-build.
   - `inputSrcs` of must-build derivations that are in the index are included.
   - Paths are looked up in rounds, only as the DFS reaches them: each round plans with what is known, taking unknown outputs as hits, and looks up the unknown paths that plan consulted. Nothing beneath a hit is looked up. After a round that mostly misses (beyond the first) when the next round would look up more paths than it did, or after 8 rounds, every remaining path of the graph is looked up at once.
4. The result is the deduplicated runtime closure of all hits, plus `roots`: hits that are not inside another hit's closure.
5. Warnings:
   - `unindexed_nixpkgs`: a final stdenv in `to_build`.
   - `close_miss`: a derivation in `to_build` whose name matches indexed paths (fewer than `close_miss_threshold`, default 100) with a different hash. Matches are counted only up to the threshold.

## Query CLI (`nfi`)

- **Commands:** `search`, `show <attr> --commit`, `bulk` (from args or a file), `minimise <installable|file|->...`.
- **Minimise input:** takes several installables, such as flake refs, `.drv` paths or anything else `nix derivation show` accepts.
  - The CLI runs `nix derivation show --recursive` on them, merges the graphs and always sends the reduced graph.
  - `-f file` or `-` takes the JSON directly.
- **Fetch hook:** set with `--fetch-hook CMD`, `NFI_FETCH_HOOK` or the config file. It applies to `minimise` and `bulk`.
  - The command is shell-split, and `{}` is replaced with the path of a temp file. When there is no `{}`, the path is appended.
  - The temp file holds one `/nix/store/...` path per line: the closure, or only the roots with `--roots-only`.
  - The hook runs synchronously and inherits stdin, stdout and stderr.
  - The CLI exits with the hook's exit status.
- **Output:** human-readable by default, or `--json`.

## Admin CLI (`nfi-admin`)

Commands: `ingest <file|url>...`, `jobs [id]`, `releases`, `prune --release/--commit/--channel`, `reindex`, all with `--wait` support.

## Testing

- **Unit tests:** format round-trips (binary and JSON), merge/split, narinfo parsing, minimise on synthetic graphs.
- **Integration tests:** Postgres and Meilisearch run as ephemeral local processes or containers, using small recorded fixtures (narinfos, store-paths, packages.json, programs.sqlite). They make no live network calls.
- **Smoke test:** an opt-in live test (`NFI_LIVE_TESTS=1`) indexes a real release, with closure sampling limited to keep it small.
