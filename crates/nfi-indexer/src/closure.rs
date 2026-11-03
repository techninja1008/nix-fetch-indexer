//! Closure indexer: store-paths.xz + mass narinfo querying.

use crate::fetch::{Fetched, Fetcher};
use crate::narinfo;
use anyhow::{Context, Result};
use futures::stream::{FuturesUnordered, StreamExt};
use nfi_interchange::{Bitset, ClosureBuilder, StoreHash, StorePath};
use std::collections::HashSet;
use std::io::Read;

pub const DEFAULT_CACHE: &str = "https://cache.nixos.org";

#[derive(Clone, Debug)]
pub struct ClosureOptions {
    pub cache_url: String,
    pub concurrency: usize,
    /// Only index the first N store paths (for smoke tests).
    pub sample: Option<usize>,
}

impl Default for ClosureOptions {
    fn default() -> Self {
        Self {
            cache_url: DEFAULT_CACHE.into(),
            concurrency: 512,
            sample: None,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct ClosureReport {
    pub roots: usize,
    pub fetched: usize,
    pub reused: usize,
    /// Roots recorded as missing: not in the cache, or with a closure that
    /// isn't (some path in it is 404).
    pub missing: usize,
}

/// Parse the decompressed contents of store-paths.xz.
pub fn parse_store_paths(xz: &[u8]) -> Result<Vec<StorePath>> {
    let mut text = String::new();
    xz2::read::XzDecoder::new(xz)
        .read_to_string(&mut text)
        .context("decompressing store-paths.xz")?;
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|l| StorePath::parse(l).with_context(|| format!("store-paths.xz line {l:?}")))
        .collect()
}

/// Index the closure of `roots` into `builder`, marking it as part of
/// `closure` (an index into the builder's closure table). Paths already
/// present in the builder are not fetched again. Roots that are not in the
/// cache, or whose closure includes (at any depth) a path that isn't, are
/// recorded as missing from the closure instead, and their closures are
/// not marked (paths in them that other roots reach still are).
///
/// This is the walk shared by every closure type: they differ only in how
/// their roots are collected.
pub async fn index_closure(
    fetcher: &Fetcher,
    opts: &ClosureOptions,
    builder: &mut ClosureBuilder,
    roots: &[StorePath],
    closure: usize,
) -> Result<ClosureReport> {
    let mut report = ClosureReport {
        roots: roots.len(),
        ..Default::default()
    };
    let bit = Bitset::single(closure);
    let mut queued: HashSet<StoreHash> = HashSet::new();
    let mut pending: Vec<StoreHash> = Vec::new();
    for r in roots {
        if builder.contains(&r.hash) {
            report.reused += 1;
        } else if queued.insert(r.hash) {
            pending.push(r.hash);
        }
    }
    let mut not_found = 0usize;
    if !pending.is_empty() {
        fetcher
            .warm_up(&format!(
                "{}/nix-cache-info",
                opts.cache_url.trim_end_matches('/')
            ))
            .await;
    }
    let started = std::time::Instant::now();
    let mut in_flight = FuturesUnordered::new();
    let total_hint = pending.len();
    loop {
        while in_flight.len() < opts.concurrency.max(1) {
            let Some(hash) = pending.pop() else { break };
            let url = narinfo::url(&opts.cache_url, &hash);
            let f = fetcher.clone();
            in_flight.push(async move { (hash, f.get_opt(&url).await) });
        }
        let Some((hash, res)) = in_flight.next().await else {
            break;
        };
        match res? {
            Fetched::Found(body) => {
                let text = String::from_utf8_lossy(&body);
                let info = narinfo::parse(&text).with_context(|| format!("narinfo for {hash}"))?;
                for r in &info.references {
                    if !builder.contains(r) && queued.insert(*r) {
                        pending.push(*r);
                    }
                }
                builder.insert(info, &Bitset::new());
                report.fetched += 1;
                if report.fetched.is_multiple_of(5000) {
                    tracing::info!(
                        fetched = report.fetched,
                        queued = pending.len() + in_flight.len(),
                        initial = total_hint,
                        "narinfo progress"
                    );
                }
            }
            // Left out of the builder: the roots whose closures include
            // it are found by `mark_closure`.
            Fetched::NotFound => not_found += 1,
        }
    }
    let secs = started.elapsed().as_secs_f64();
    tracing::info!(
        fetched = report.fetched,
        not_found,
        secs = format!("{secs:.1}"),
        per_sec = format!("{:.0}", (report.fetched + not_found) as f64 / secs.max(1e-9)),
        "narinfo fetching finished"
    );
    let unmarked: HashSet<StoreHash> = builder
        .mark_closure(roots.iter().map(|r| r.hash), closure)
        .into_iter()
        .collect();
    for r in roots.iter().filter(|r| unmarked.contains(&r.hash)) {
        builder.add_missing(r.hash, &r.name, &bit);
    }
    report.missing = unmarked.len();
    Ok(report)
}
