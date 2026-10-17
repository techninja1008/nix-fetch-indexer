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
/// `release`. Paths already present in the builder are not fetched again.
pub async fn index_closure(
    fetcher: &Fetcher,
    opts: &ClosureOptions,
    builder: &mut ClosureBuilder,
    roots: &[StorePath],
    release: usize,
) -> Result<ClosureReport> {
    let mut report = ClosureReport {
        roots: roots.len(),
        ..Default::default()
    };
    let bit = Bitset::single(release);
    let mut queued: HashSet<StoreHash> = HashSet::new();
    let mut pending: Vec<(StoreHash, String)> = Vec::new();
    for r in roots {
        if builder.contains(&r.hash) {
            report.reused += 1;
        } else if queued.insert(r.hash) {
            pending.push((r.hash, r.name.clone()));
        }
    }
    let mut missing_roots: Vec<(StoreHash, String)> = Vec::new();
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
            let Some((hash, name)) = pending.pop() else { break };
            let url = narinfo::url(&opts.cache_url, &hash);
            let f = fetcher.clone();
            in_flight.push(async move { (hash, name, f.get_opt(&url).await) });
        }
        let Some((hash, name, res)) = in_flight.next().await else {
            break;
        };
        match res? {
            Fetched::Found(body) => {
                let text = String::from_utf8_lossy(&body);
                let info = narinfo::parse(&text).with_context(|| format!("narinfo for {hash}"))?;
                for r in &info.references {
                    if !builder.contains(r) && queued.insert(*r) {
                        // Name is filled in when its narinfo arrives.
                        pending.push((*r, String::new()));
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
            Fetched::NotFound => {
                report.missing += 1;
                missing_roots.push((hash, name));
            }
        }
    }
    let secs = started.elapsed().as_secs_f64();
    tracing::info!(
        fetched = report.fetched,
        secs = format!("{secs:.1}"),
        per_sec = format!("{:.0}", (report.fetched + report.missing) as f64 / secs.max(1e-9)),
        "narinfo fetching finished"
    );
    let root_hashes: Vec<StoreHash> = roots.iter().map(|r| r.hash).collect();
    let missing_set: HashSet<StoreHash> = missing_roots.iter().map(|(h, _)| *h).collect();
    builder.mark_closure(
        root_hashes.into_iter().filter(|h| !missing_set.contains(h)),
        release,
    );
    for (h, name) in missing_roots {
        builder.add_missing(h, &name, &bit);
    }
    Ok(report)
}
