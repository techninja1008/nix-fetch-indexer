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

/// Which of the given full store paths are in the closure index.
pub async fn cached_paths(pool: &PgPool, paths: &BTreeSet<String>) -> Result<BTreeSet<String>> {
    let parsed: Vec<StorePath> = paths.iter().filter_map(|p| StorePath::parse(p).ok()).collect();
    let hashes: Vec<String> = parsed.iter().map(|p| p.hash.to_string()).collect();
    let found: Vec<(String, String)> =
        sqlx::query_as("SELECT hash, name FROM store_paths WHERE hash = ANY($1)")
            .bind(&hashes)
            .fetch_all(pool)
            .await?;
    let found: HashMap<String, String> = found.into_iter().collect();
    Ok(parsed
        .into_iter()
        .filter(|p| found.get(&p.hash.to_string()).is_some_and(|n| *n == p.name))
        .map(|p| p.full())
        .collect())
}

/// Runtime closure of the given (cached) store paths, plus the edges within it.
pub async fn closure(
    pool: &PgPool,
    paths: &BTreeSet<String>,
) -> Result<(Vec<ClosureNode>, Vec<(String, String)>)> {
    let hashes: Vec<String> = paths
        .iter()
        .filter_map(|p| StorePath::parse(p).ok())
        .map(|p| p.hash.to_string())
        .collect();
    if hashes.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    let rows: Vec<(i64, String, String, Option<i64>, Option<i64>, Vec<i64>)> = sqlx::query_as(
        "WITH RECURSIVE c(id) AS (
             SELECT id FROM store_paths WHERE hash = ANY($1)
             UNION
             SELECT unnest(s.refs) FROM store_paths s JOIN c ON s.id = c.id
         )
         SELECT s.id, s.hash, s.name, s.nar_size, s.file_size, s.refs
           FROM c JOIN store_paths s ON s.id = c.id ORDER BY s.hash",
    )
    .bind(&hashes)
    .fetch_all(pool)
    .await?;
    let by_id: HashMap<i64, String> = rows
        .iter()
        .map(|(id, hash, name, ..)| (*id, format!("/nix/store/{hash}-{name}")))
        .collect();
    let mut edges = Vec::new();
    for (id, _, _, _, _, refs) in &rows {
        for r in refs {
            if let (Some(a), Some(b)) = (by_id.get(id), by_id.get(r)) {
                edges.push((a.clone(), b.clone()));
            }
        }
    }
    let nodes = rows
        .into_iter()
        .map(|(id, _, _, nar, file, _)| ClosureNode {
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

pub async fn minimise(pool: &PgPool, graph: &ReducedGraph) -> Result<MinimiseResponse> {
    let cached = cached_paths(pool, &graph.all_paths()).await?;
    let plan = nfi_api::minimise::plan(graph, |p| cached.contains(p));
    let warnings = nfi_api::minimise::anchor_warnings(graph, |p| cached.contains(p));
    let (nodes, edges) = closure(pool, &plan.hits).await?;
    let roots = nfi_api::minimise::roots(&plan.hits, &edges);
    let (nar_size, file_size) = sizes(&nodes);
    Ok(MinimiseResponse {
        requested: plan.requested,
        to_build: plan.to_build,
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

fn with_cached(outs: BTreeMap<String, String>, cached: &BTreeSet<String>) -> BTreeMap<String, OutputInfo> {
    outs.into_iter()
        .map(|(o, p)| {
            let c = cached.contains(&p);
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
            o.cached = cached.contains(&o.path);
        }
        if it.status == ItemStatus::Ok && it.outputs.values().any(|o| !o.cached) {
            it.status = ItemStatus::NotCached;
        }
    }
    let (nodes, _) = closure(pool, &cached).await?;
    let (nar_size, file_size) = sizes(&nodes);
    Ok(BulkResponse {
        items,
        closure: nodes.into_iter().map(|n| n.path).collect(),
        nar_size,
        file_size,
    })
}
