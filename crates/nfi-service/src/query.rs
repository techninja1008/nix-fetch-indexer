//! Lookup, bulk lookup and minimise queries.

use anyhow::Result;
use nfi_api::drv::ReducedGraph;
use nfi_api::*;
use nfi_interchange::StorePath;
use sqlx::PgPool;
use std::collections::{BTreeMap, BTreeSet, HashMap};

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct ReleaseRow {
    pub id: i32,
    pub name: String,
    pub commit: String,
    pub channel: String,
    pub series: String,
}

impl ReleaseRow {
    pub fn to_ref(&self) -> ReleaseRef {
        ReleaseRef {
            name: self.name.clone(),
            commit: self.commit.clone(),
            channel: self.channel.clone(),
            series: self.series.clone(),
        }
    }
}

/// The index violates an invariant that ingest and prune maintain (see
/// `nfi-admin check`). Queries fail rather than return partial answers.
#[derive(Debug, thiserror::Error)]
#[error("index inconsistency: {0}; run `nfi-admin check` for details")]
pub struct Inconsistent(pub String);

pub enum CommitLookup {
    Found(ReleaseRow),
    Unknown,
    Ambiguous,
}

/// Resolve a full commit hash or unique prefix to its release.
pub async fn resolve_commit(pool: &PgPool, commit: &str) -> Result<CommitLookup> {
    let c = commit.trim().to_lowercase();
    if c.is_empty() || !c.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Ok(CommitLookup::Unknown);
    }
    let rows: Vec<ReleaseRow> = sqlx::query_as(
        "SELECT id, name, commit, channel, series FROM releases WHERE commit LIKE $1 || '%' ORDER BY id LIMIT 2",
    )
    .bind(&c)
    .fetch_all(pool)
    .await?;
    Ok(match rows.len() {
        0 => CommitLookup::Unknown,
        1 => CommitLookup::Found(rows.into_iter().next().expect("one row")),
        _ => CommitLookup::Ambiguous,
    })
}

#[derive(Clone, Debug)]
pub struct ClosureNode {
    pub id: i64,
    pub path: String,
    pub nar_size: Option<i64>,
    pub file_size: Option<i64>,
}

/// Which of the given full store paths are in the closure index, with their ids.
pub async fn cached_paths(pool: &PgPool, paths: &BTreeSet<String>) -> Result<BTreeMap<String, i64>> {
    let parsed: Vec<StorePath> = paths.iter().filter_map(|p| StorePath::parse(p).ok()).collect();
    if parsed.is_empty() {
        return Ok(BTreeMap::new());
    }
    let hashes: Vec<String> = parsed.iter().map(|p| p.hash.to_string()).collect();
    let found: Vec<(i64, String, String, i32)> =
        sqlx::query_as("SELECT id, hash, name, cardinality(closures) FROM store_paths WHERE hash = ANY($1)")
            .bind(&hashes)
            .fetch_all(pool)
            .await?;
    if let Some((_, hash, name, _)) = found.iter().find(|(.., n)| *n == 0) {
        return Err(Inconsistent(format!("store path {hash}-{name} belongs to no closure")).into());
    }
    let found: HashMap<String, (i64, String)> = found.into_iter().map(|(id, h, n, _)| (h, (id, n))).collect();
    Ok(parsed
        .into_iter()
        .filter_map(|p| {
            let (id, name) = found.get(&p.hash.to_string())?;
            (*name == p.name).then(|| (p.full(), *id))
        })
        .collect())
}

/// Runtime closure of the given cached store paths (by id, from
/// [`cached_paths`]), plus the edges within it.
pub async fn closure(pool: &PgPool, ids: &[i64]) -> Result<(Vec<ClosureNode>, Vec<(String, String)>)> {
    if ids.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    // Postgres estimates each step of a recursive query from a fixed
    // multiple of the one before, so it overestimates closures and joins
    // them, in the recursive step and the final select, with a sequential
    // scan of store_paths. The lateral subqueries (kept from being pulled up
    // by OFFSET 0) look each path up by id instead. JIT would compile for the
    // inflated estimate, so it is off.
    let mut tx = pool.begin().await?;
    sqlx::query("SET LOCAL jit = off").execute(&mut *tx).await?;
    let rows: Vec<(i64, String, String, Option<i64>, Option<i64>, Vec<i64>, i32)> = sqlx::query_as(
        "WITH RECURSIVE c(id) AS (
             SELECT unnest($1::bigint[])
             UNION
             SELECT unnest(s.refs) FROM c, LATERAL (SELECT refs FROM store_paths s WHERE s.id = c.id OFFSET 0) s
         )
         SELECT s.id, s.hash, s.name, s.nar_size, s.file_size, s.refs, cardinality(s.closures)
           FROM c, LATERAL (SELECT * FROM store_paths s WHERE s.id = c.id OFFSET 0) s
          ORDER BY s.hash",
    )
    .bind(ids)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    let by_id: HashMap<i64, String> = rows
        .iter()
        .map(|(id, hash, name, ..)| (*id, format!("/nix/store/{hash}-{name}")))
        .collect();
    // References to ids without a row would otherwise silently shrink the
    // closure (the recursive join drops them).
    let mut edges = Vec::new();
    for (id, _, _, _, _, refs, closures) in &rows {
        let path = &by_id[id];
        if *closures == 0 {
            return Err(Inconsistent(format!("store path {path} belongs to no closure")).into());
        }
        for r in refs {
            let Some(target) = by_id.get(r) else {
                return Err(Inconsistent(format!("{path} references missing store path id {r}")).into());
            };
            edges.push((path.clone(), target.clone()));
        }
    }
    let nodes = rows
        .into_iter()
        .map(|(id, _, _, nar, file, _, _)| ClosureNode {
            id,
            path: by_id[&id].clone(),
            nar_size: nar,
            file_size: file,
        })
        .collect();
    Ok((nodes, edges))
}

fn sizes(nodes: &[ClosureNode]) -> (u64, u64) {
    nodes.iter().fold((0, 0), |(n, f), x| {
        (
            n + x.nar_size.unwrap_or(0).max(0) as u64,
            f + x.file_size.unwrap_or(0).max(0) as u64,
        )
    })
}

/// Number of indexed store paths with each of `names`, counting at most
/// `limit` per name: generic names (`source`) are shared by a large part of
/// the index.
async fn indexed_names(pool: &PgPool, names: &BTreeSet<&str>, limit: u64) -> Result<HashMap<String, u64>> {
    if names.is_empty() {
        return Ok(HashMap::new());
    }
    let names: Vec<&str> = names.iter().copied().collect();
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT n, (SELECT count(*) FROM (SELECT FROM store_paths WHERE name = n LIMIT $2) l)
           FROM unnest($1::text[]) n",
    )
    .bind(&names)
    .bind(i64::try_from(limit).unwrap_or(i64::MAX))
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|(n, c)| (n, c as u64)).collect())
}

/// Lookup rounds before minimise looks up every remaining path of the graph
/// at once. Graphs built on an indexed release need a few rounds: one per
/// level of derivations to build above the hits.
const MINIMISE_ROUNDS: usize = 8;

pub async fn minimise(
    pool: &PgPool,
    graph: &ReducedGraph,
    close_miss_threshold: u64,
) -> Result<MinimiseResponse> {
    use nfi_api::minimise as m;
    // Look up only the paths the plan consults, in rounds: nothing beneath a
    // hit needs looking up, and that is most of a graph built on an indexed
    // release.
    let mut planner = m::Planner::new(graph);
    let mut cached: BTreeMap<String, i64> = BTreeMap::new();
    let mut round = 0;
    let mut mostly_missing = false;
    let mut last_asked = 0;
    let plan = loop {
        let (plan, unknown) = planner.step();
        if unknown.is_empty() {
            break plan;
        }
        round += 1;
        // A round that mostly missed (after the first, whose requested
        // outputs are rarely cached), followed by a larger one, suggests an
        // unindexed nixpkgs: the plan would descend through most of the graph
        // a level per round. Misses that lead to fewer paths, such as
        // vendored crates above their fetches, are followed round by round.
        let ask = if round < MINIMISE_ROUNDS && !(mostly_missing && unknown.len() > last_asked) {
            unknown
        } else {
            planner.remaining()
        };
        let found = cached_paths(pool, &ask).await?;
        mostly_missing = round > 1 && found.len() * 2 < ask.len();
        last_asked = ask.len();
        planner.learn(&ask, &|p| found.contains_key(p));
        cached.extend(found);
    };
    let hit_ids: Vec<i64> = plan.hits.iter().map(|p| cached[p]).collect();
    let mut warnings = m::anchor_warnings(&plan);
    let names: BTreeSet<&str> = if close_miss_threshold > 0 {
        plan.to_build.iter().map(|d| m::drv_name(d)).collect()
    } else {
        BTreeSet::new()
    };
    let (indexed, (nodes, edges)) = tokio::try_join!(
        indexed_names(pool, &names, close_miss_threshold),
        closure(pool, &hit_ids),
    )?;
    warnings.extend(m::close_miss_warnings(
        &plan,
        |n| indexed.get(n).copied().unwrap_or(0),
        close_miss_threshold,
    ));
    let roots = nfi_api::minimise::roots(&plan.hits, &edges);
    let (nar_size, file_size) = sizes(&nodes);
    Ok(MinimiseResponse {
        requested: plan.requested,
        to_build: plan.to_build,
        to_fetch: plan.to_fetch,
        roots,
        closure: nodes.into_iter().map(|n| n.path).collect(),
        unresolved: plan.unresolved,
        nar_size,
        file_size,
        warnings,
    })
}

#[derive(sqlx::FromRow)]
struct PackageRow {
    name: String,
    pname: Option<String>,
    version: Option<String>,
    description: Option<String>,
    homepage: Option<String>,
    license: Vec<String>,
    main_program: Option<String>,
    broken: bool,
    unfree: bool,
}

async fn package_row(pool: &PgPool, release: i32, attr: &str, system: &str) -> Result<Option<PackageRow>> {
    Ok(sqlx::query_as(
        "SELECT p.name, p.pname, p.version, p.description, p.homepage, p.license, p.main_program, p.broken, p.unfree
         FROM packages p JOIN package_releases pr ON pr.package_id = p.id
         WHERE pr.release_id = $1 AND p.attr = $2 AND p.system = $3 LIMIT 1",
    )
    .bind(release)
    .bind(attr)
    .bind(system)
    .fetch_optional(pool)
    .await?)
}

async fn outputs(
    pool: &PgPool,
    release: i32,
    attr: &str,
    system: &str,
) -> Result<(BTreeMap<String, String>, Option<String>)> {
    let rows: Vec<(String, String, Option<String>)> = sqlx::query_as(
        "SELECT output, path, drv_path FROM package_outputs WHERE release_id = $1 AND attr = $2 AND system = $3",
    )
    .bind(release)
    .bind(attr)
    .bind(system)
    .fetch_all(pool)
    .await?;
    let drv = rows.iter().find_map(|r| r.2.clone());
    Ok((rows.into_iter().map(|(o, p, _)| (o, p)).collect(), drv))
}

fn with_cached(
    outs: BTreeMap<String, String>,
    cached: &BTreeMap<String, i64>,
) -> BTreeMap<String, OutputInfo> {
    outs.into_iter()
        .map(|(o, p)| {
            let c = cached.contains_key(&p);
            (o, OutputInfo { path: p, cached: c })
        })
        .collect()
}

pub enum LookupError {
    UnknownCommit,
    AmbiguousCommit,
    NotFound,
}

pub async fn lookup(
    pool: &PgPool,
    attr: &str,
    commit: &str,
    system: &str,
) -> Result<std::result::Result<PackageInfo, LookupError>> {
    let release = match resolve_commit(pool, commit).await? {
        CommitLookup::Found(r) => r,
        CommitLookup::Unknown => return Ok(Err(LookupError::UnknownCommit)),
        CommitLookup::Ambiguous => return Ok(Err(LookupError::AmbiguousCommit)),
    };
    let pkg = package_row(pool, release.id, attr, system).await?;
    let (outs, drv_path) = outputs(pool, release.id, attr, system).await?;
    let programs: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM programs WHERE release_id = $1 AND attr = $2 AND system = $3 ORDER BY name",
    )
    .bind(release.id)
    .bind(attr)
    .bind(system)
    .fetch_all(pool)
    .await?;
    if pkg.is_none() && outs.is_empty() && programs.is_empty() {
        return Ok(Err(LookupError::NotFound));
    }
    let cached = cached_paths(pool, &outs.values().cloned().collect()).await?;
    let p = pkg.unwrap_or(PackageRow {
        name: String::new(),
        pname: None,
        version: None,
        description: None,
        homepage: None,
        license: vec![],
        main_program: None,
        broken: false,
        unfree: false,
    });
    Ok(Ok(PackageInfo {
        attr: attr.to_string(),
        system: system.to_string(),
        release: release.to_ref(),
        name: Some(p.name).filter(|n| !n.is_empty()),
        pname: p.pname,
        version: p.version,
        description: p.description,
        homepage: p.homepage,
        license: p.license,
        main_program: p.main_program,
        broken: p.broken,
        unfree: p.unfree,
        drv_path,
        outputs: with_cached(outs, &cached),
        programs,
    }))
}

/// Select the requested outputs: default `out` (or all when there is no `out`),
/// `*` for all.
fn select_outputs(all: BTreeMap<String, String>, wanted: &Option<Vec<String>>) -> BTreeMap<String, String> {
    match wanted {
        Some(w) if w.iter().any(|o| o == "*") => all,
        Some(w) => all.into_iter().filter(|(o, _)| w.contains(o)).collect(),
        None if all.contains_key("out") => all.into_iter().filter(|(o, _)| o == "out").collect(),
        None => all,
    }
}

pub async fn bulk(pool: &PgPool, req: &BulkRequest) -> Result<BulkResponse> {
    let mut commits: HashMap<String, CommitLookup> = HashMap::new();
    let mut items = Vec::with_capacity(req.items.len());
    let mut wanted_paths = BTreeSet::new();
    for it in &req.items {
        let system = it.system.clone().unwrap_or_else(|| DEFAULT_SYSTEM.to_string());
        if !commits.contains_key(&it.commit) {
            commits.insert(it.commit.clone(), resolve_commit(pool, &it.commit).await?);
        }
        let mut res = BulkItemResult {
            attr: it.attr.clone(),
            commit: it.commit.clone(),
            system: system.clone(),
            status: ItemStatus::Ok,
            release: None,
            outputs: BTreeMap::new(),
        };
        let release = match &commits[&it.commit] {
            CommitLookup::Found(r) => r.clone(),
            CommitLookup::Unknown => {
                res.status = ItemStatus::UnknownCommit;
                items.push(res);
                continue;
            }
            CommitLookup::Ambiguous => {
                res.status = ItemStatus::AmbiguousCommit;
                items.push(res);
                continue;
            }
        };
        res.release = Some(release.name.clone());
        res.commit = release.commit.clone();
        let (outs, _) = outputs(pool, release.id, &it.attr, &system).await?;
        if outs.is_empty() {
            res.status = if package_row(pool, release.id, &it.attr, &system).await?.is_some() {
                ItemStatus::NoOutpaths
            } else {
                ItemStatus::UnknownAttr
            };
            items.push(res);
            continue;
        }
        let sel = select_outputs(outs, &it.outputs);
        wanted_paths.extend(sel.values().cloned());
        res.outputs = sel
            .into_iter()
            .map(|(o, p)| {
                (
                    o,
                    OutputInfo {
                        path: p,
                        cached: false,
                    },
                )
            })
            .collect();
        items.push(res);
    }
    let cached = cached_paths(pool, &wanted_paths).await?;
    for it in &mut items {
        for o in it.outputs.values_mut() {
            o.cached = cached.contains_key(&o.path);
        }
        if it.status == ItemStatus::Ok && it.outputs.values().any(|o| !o.cached) {
            it.status = ItemStatus::NotCached;
        }
    }
    let ids: Vec<i64> = cached.values().copied().collect();
    let (nodes, _) = closure(pool, &ids).await?;
    let (nar_size, file_size) = sizes(&nodes);
    Ok(BulkResponse {
        items,
        closure: nodes.into_iter().map(|n| n.path).collect(),
        nar_size,
        file_size,
    })
}
