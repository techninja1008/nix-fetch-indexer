# nix-fetch-indexer — design

Tooling, written in Rust, for indexing the binary-cache closure of NixOS / nixpkgs releases and querying it.
The main goal is to find the minimal set of cached store paths that is useful for building a given derivation.

## Workspace layout

A single Cargo workspace:

| crate | kind | purpose |
|---|---|---|
| `nfi-interchange` | lib | Interchange bundle data model, CBOR + compression codec, JSON codec, merge/split/filter |
| `nfi-indexer` | bin | Indexers (`closure`, `packages`, `outpaths`, `programs`), plus offline bundle tooling (`merge`, `split`, `convert`, `inspect`) |
| `nfi-service` | bin | Query service (axum HTTP/JSON, Postgres, Meilisearch) |
| `nfi` | bin | Query CLI (non-administrative endpoints, fetch hook, flake-ref resolution) |
| `nfi-admin` | bin | Admin CLI (ingest, list releases, prune, reindex, job status) |
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
  releases: [Release],              // release table; bit i of every membership bitset refers to releases[i]
  closure:  Option<ClosureSection>,
  packages: Option<PackagesSection>,
  outpaths: Option<OutpathsSection>,
  programs: Option<ProgramsSection>,
}
Release { name, commit, channel, series, url, datasets: [closure|packages|outpaths|programs] }

ClosureSection {
  paths: [ClosurePath { hash, name, references: [path index], nar_size, file_size, deriver: Option<String>, releases: Bitset }],
  missing: [MissingPath { hash, name, releases: Bitset }]   // in store-paths.xz but 404 on the cache
}
PackagesSection { entries: [Package { attr, name, pname, version, system, description, homepage, license, main_program, broken, unfree, releases: Bitset }] }
OutpathsSection { entries: [Outpaths { attr, system, drv_path, outputs: {name: store path}, releases: Bitset }] }
ProgramsSection { entries: [Program { name, system, attr, releases: Bitset }] }
```

- `datasets` on a release records which sections contain data for it.
  - A release can be present in a section and still have zero entries.
- **References** are indices into `paths`, which keeps the binary form compact. Paths outside the indexed set do not exist, because the closure is followed transitively.
- **Bitsets** are encoded as little-endian byte strings.
- **Binary form:**
  - Layout: magic `NFIB`, a one-byte version, a one-byte compression id, then the compressed CBOR body.
  - Compression ids: `0` none, `1` zstd, `2` xz, `3` brotli.
  - Benchmark (`nfi-indexer bench-compression`): per algorithm, compressed size and encode/decode time.
  - Default: **zstd level 19** for indexer output.
- **JSON form:** the same data model.
  - Structurally identical, with bitsets as hex strings and references as index arrays.
  - It is detected by a leading `{`.
- **Merge:** union the release tables by name, remap the bitsets, and union entries by key:
  - closure: hash
  - packages: (attr, version, system, metadata)
  - outpaths: (attr, system, drv_path)
  - programs: (name, system, attr)
- **Split / filter:** select a subset of releases and/or sections, then drop the entries whose bitset becomes empty.

## Indexers (`nfi-indexer`)

- **Input:** each indexer takes either `--release-url <full URL>` or `--channel-list <prefix>`. The second option enumerates every release under a prefix via the S3 listing API, with optional `--limit` and `--since`.
- **Output:** a bundle written with `-o <file>` and `--format binary|json`.
- **`closure`**
  - Fetch `store-paths.xz`, then mass-query narinfos over HTTP/2 with bounded concurrency (default 64).
  - Retries back off on 429/5xx.
  - The indexer follows references transitively.
  - Paths that return 404 are recorded in `missing`.
  - `--seed <bundle>` reuses known hashes: store paths are immutable, so only new hashes are fetched. In channel-list mode, each release seeds the next.
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

### Postgres schema (sketch)

```
releases(id, name unique, commit unique, channel, series, url, indexed_at)
release_datasets(release_id, dataset)
store_paths(id bigserial, hash char(32) unique, name, nar_size, file_size, deriver)
store_path_refs(path_id, ref_id)                    -- edges
store_path_releases(path_id, release_id)
missing_paths(hash, name, release_id)
packages(id, attr, version, system, name, pname, description, homepage, license, main_program, broken, unfree)
package_releases(package_id, release_id)
package_outputs(attr, system, release_id, output, path_hash, drv_path)   -- joined from outpaths at ingest
programs(name, system, attr, release_id)
jobs(id, kind, status, params jsonb, staged_blob oid/bytea, progress, error, created_at, started_at, finished_at)
```

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
- `GET /admin/v1/releases` lists releases and their datasets.
- `POST /admin/v1/prune` takes `{releases?:[name], commits?:[hash], channels?:[name]}` and returns a job id.
- `POST /admin/v1/reindex` returns a job id.

### Minimise algorithm

1. Reduce the input to a derivation graph.
2. The **requested** derivations are those that no other derivation in the input references through `inputDrvs`.
3. DFS from the requested derivations.
   - A derivation is a **hit** when every output it needs is a known store path in the index.
     - "Needed" means the outputs its dependants consume, or all outputs for a requested derivation.
     - Paths present in any indexed release count, and paths in `missing` do not.
   - On a hit, record those paths and stop descending.
   - Otherwise the derivation must be built: add it to `to_build` and recurse into its `inputDrvs` with the outputs they consume.
     - Content-addressed or floating outputs with no known path always count as must-build.
   - `inputSrcs` of must-build derivations that are in the index are included.
4. The result is the deduplicated runtime closure of all hits, plus `roots`: hits that are not inside another hit's closure.

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
