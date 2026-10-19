//! Orchestrates indexing of one or more releases into a bundle.

use crate::closure::{self, ClosureOptions};
use crate::fetch::{Fetched, Fetcher};
use crate::outpaths::{self, OutpathsOptions};
use crate::probes::{self, HELPER_PROBES, HELPERS_DIRECT, HELPERS_FULL, ProbeOptions};
use crate::release::ReleaseLocation;
use crate::{packages, programs, tarball};
use anyhow::{Context, Result};
use nfi_interchange::*;

#[derive(Clone, Debug)]
pub struct IndexOptions {
    pub datasets: Vec<Dataset>,
    /// Closure types to index for the closure dataset (`store-paths`,
    /// `helpers-direct`, `helpers-full`).
    pub closure_types: Vec<String>,
    pub closure: ClosureOptions,
    pub probes: ProbeOptions,
    pub outpaths: OutpathsOptions,
    /// Keep the seed bundle's releases in the output.
    pub include_seed: bool,
}

/// Timings and counts for one indexed release.
#[derive(Clone, Debug, Default)]
pub struct ReleaseReport {
    pub name: String,
    pub commit: String,
    /// Per closure type.
    pub closures: Vec<(String, closure::ClosureReport)>,
    pub closure_secs: f64,
    pub packages: Option<usize>,
    pub packages_secs: f64,
    pub programs: Option<usize>,
    pub programs_secs: f64,
    pub outpaths: Option<usize>,
    pub outpaths_secs: f64,
    /// Helper probes that failed to evaluate.
    pub eval_errors: Option<usize>,
}

/// What an indexing run did, for logging a summary.
#[derive(Clone, Debug, Default)]
pub struct IndexReport {
    pub releases: Vec<ReleaseReport>,
    /// Building the final bundle (closure finalisation, merge/filter).
    pub assemble_secs: f64,
    pub total_secs: f64,
}

fn secs(t: std::time::Instant) -> f64 {
    t.elapsed().as_secs_f64()
}

/// Index `locations` (in order) into a new bundle. When `seed` is given its
/// closure section is used to avoid re-fetching known narinfos.
pub async fn index_releases(
    fetcher: &Fetcher,
    locations: &[ReleaseLocation],
    opts: &IndexOptions,
    seed: Option<Bundle>,
) -> Result<Bundle> {
    Ok(index_releases_with_report(fetcher, locations, opts, seed)
        .await?
        .0)
}

/// Like [`index_releases`], also returning per-phase timings and counts.
pub async fn index_releases_with_report(
    fetcher: &Fetcher,
    locations: &[ReleaseLocation],
    opts: &IndexOptions,
    seed: Option<Bundle>,
) -> Result<(Bundle, IndexReport)> {
    let run_started = std::time::Instant::now();
    let mut report = IndexReport::default();
    let mut bundle = Bundle::default();
    let mut builder = ClosureBuilder::new();
    let mut seed_releases = Vec::new();
    if let Some(seed) = &seed {
        for r in &seed.releases {
            seed_releases.push(r.name.clone());
        }
        let map: Vec<usize> = seed
            .releases
            .iter()
            .map(|r| bundle.ensure_release(r.clone()))
            .collect();
        if let Some(c) = &seed.closure {
            builder.extend_section(c, &|i| map.get(i).copied());
        }
    }
    let seed_sections = seed.clone();

    // Deduplicated as each release is added: consecutive releases share most
    // entries, and a whole channel would otherwise hold ~100x the entries.
    let mut package_entries = ops::Dedup::packages();
    let mut outpath_entries = ops::Dedup::outpaths();
    let mut program_entries = ops::Dedup::programs();
    let mut eval_errors = EvalErrorsSection::default();
    let mut new_releases = Vec::new();

    for loc in locations {
        let mut release = loc.resolve(fetcher).await?;
        tracing::info!(release = %release.name, commit = %release.commit, "indexing release");
        release.datasets = Vec::new();
        let idx = bundle.ensure_release(release.clone());
        new_releases.push(release.name.clone());
        let mut rr = ReleaseReport {
            name: release.name.clone(),
            commit: release.commit.clone(),
            ..Default::default()
        };

        // The nixexprs tarball, fetched (and unpacked) at most once.
        let mut tar: Option<bytes::Bytes> = None;
        let mut unpacked: Option<(tempfile::TempDir, std::path::PathBuf)> = None;

        // Helper probes are evaluated once per release, for the helper
        // closure types and for the eval-errors dataset.
        let closure_probes = opts.datasets.contains(&Dataset::Closure)
            && opts
                .closure_types
                .iter()
                .any(|t| t == HELPERS_DIRECT || t == HELPERS_FULL);
        let probe_roots = if closure_probes || opts.datasets.contains(&Dataset::EvalErrors) {
            let dir = unpack(fetcher, loc, &mut tar, &mut unpacked).await?;
            Some(
                probes::run_probes(dir, &opts.probes)
                    .await
                    .with_context(|| format!("running helper probes for {}", release.name))?,
            )
        } else {
            None
        };

        if opts.datasets.contains(&Dataset::Closure) {
            let t = std::time::Instant::now();
            for ct in &opts.closure_types {
                let roots = match ct.as_str() {
                    STORE_PATHS => {
                        let xz = fetcher.get(&loc.file("store-paths.xz")).await?;
                        let mut roots = closure::parse_store_paths(&xz)?;
                        if let Some(n) = opts.closure.sample {
                            roots.truncate(n);
                        }
                        roots
                    }
                    HELPERS_DIRECT | HELPERS_FULL => {
                        let pr = probe_roots.as_ref().expect("probes run for helper closures");
                        let set = if ct == HELPERS_DIRECT {
                            &pr.direct
                        } else {
                            &pr.full
                        };
                        set.iter().cloned().collect()
                    }
                    other => anyhow::bail!(
                        "unknown closure type {other:?} (expected {STORE_PATHS}, {HELPERS_DIRECT} or {HELPERS_FULL})"
                    ),
                };
                let cidx = builder.ensure_closure(idx, ct);
                let report =
                    closure::index_closure(fetcher, &opts.closure, &mut builder, &roots, cidx).await?;
                tracing::info!(
                    release = %release.name,
                    closure_type = %ct,
                    roots = report.roots,
                    fetched = report.fetched,
                    reused = report.reused,
                    missing = report.missing,
                    "closure indexed"
                );
                rr.closures.push((ct.clone(), report));
            }
            rr.closure_secs = secs(t);
            bundle.releases[idx].add_dataset(Dataset::Closure);
        }
        if opts.datasets.contains(&Dataset::EvalErrors) {
            let pr = probe_roots.as_ref().expect("probes run for eval errors");
            let e = eval_errors.ensure_eval(idx as u32, HELPER_PROBES);
            for (attr, system) in &pr.errors {
                eval_errors.add_error(attr, system, e);
            }
            rr.eval_errors = Some(pr.errors.len());
            bundle.releases[idx].add_dataset(Dataset::EvalErrors);
        }
        if opts.datasets.contains(&Dataset::Packages) {
            let t = std::time::Instant::now();
            let br = fetcher.get(&loc.file("packages.json.br")).await?;
            let entries = packages::parse_packages_json_br(&br, idx)?;
            rr.packages_secs = secs(t);
            rr.packages = Some(entries.len());
            tracing::info!(
                release = %release.name,
                packages = entries.len(),
                secs = format!("{:.1}", rr.packages_secs),
                "packages indexed"
            );
            package_entries.extend(entries);
            bundle.releases[idx].add_dataset(Dataset::Packages);
        }
        {
            if opts.datasets.contains(&Dataset::Programs) {
                let t = std::time::Instant::now();
                let tar = fetch_tar(fetcher, loc, &mut tar).await?;
                match tarball::read_top_level_file(tar, "programs.sqlite")? {
                    Some(db) => {
                        let entries = programs::read_programs_db(&db, idx)?;
                        rr.programs_secs = secs(t);
                        rr.programs = Some(entries.len());
                        tracing::info!(
                            release = %release.name,
                            programs = entries.len(),
                            secs = format!("{:.1}", rr.programs_secs),
                            "programs indexed"
                        );
                        program_entries.extend(entries);
                    }
                    None => {
                        rr.programs = Some(0);
                        tracing::warn!(release = %release.name, "no programs.sqlite in nixexprs tarball")
                    }
                }
                bundle.releases[idx].add_dataset(Dataset::Programs);
            }
            if opts.datasets.contains(&Dataset::Outpaths) {
                let t = std::time::Instant::now();
                let top = unpack(fetcher, loc, &mut tar, &mut unpacked).await?;
                let entries = outpaths::evaluate(top, &opts.outpaths, idx)
                    .await
                    .with_context(|| format!("evaluating {}", release.name))?;
                rr.outpaths_secs = secs(t);
                rr.outpaths = Some(entries.len());
                tracing::info!(
                    release = %release.name,
                    outpaths = entries.len(),
                    secs = format!("{:.1}", rr.outpaths_secs),
                    "out paths indexed"
                );
                outpath_entries.extend(entries);
                bundle.releases[idx].add_dataset(Dataset::Outpaths);
            }
        }
        report.releases.push(rr);
    }

    let t = std::time::Instant::now();
    tracing::info!("assembling bundle");

    if opts.datasets.contains(&Dataset::Closure)
        || seed_sections.as_ref().is_some_and(|s| s.closure.is_some())
    {
        bundle.closure = Some(builder.finish());
    }
    if opts.datasets.contains(&Dataset::Packages) {
        bundle.packages = Some(PackagesSection {
            entries: package_entries.into_entries(),
        });
    }
    if opts.datasets.contains(&Dataset::Outpaths) {
        bundle.outpaths = Some(OutpathsSection {
            entries: outpath_entries.into_entries(),
        });
    }
    if opts.datasets.contains(&Dataset::Programs) {
        bundle.programs = Some(ProgramsSection {
            entries: program_entries.into_entries(),
        });
    }
    if opts.datasets.contains(&Dataset::EvalErrors) {
        let mut b = EvalErrorsBuilder::default();
        b.extend_section(&eval_errors, &Some);
        bundle.eval_errors = Some(b.finish());
    }

    let bundle = if let (true, Some(seed)) = (opts.include_seed, seed_sections) {
        // Bring along the seed's non-closure sections too.
        let seed_rest = Bundle {
            closure: None,
            ..seed
        };
        ops::merge([bundle, seed_rest])
    } else {
        // Merging a single bundle deduplicates package/program/outpath entries
        // across its releases. The closure is already deduplicated by the
        // builder, so keep it out of the merge (it is by far the largest
        // section); a single-input merge keeps the release table order.
        let mut b = if seed_releases.is_empty() {
            bundle
        } else {
            ops::filter(&bundle, |r| new_releases.contains(&r.name), None, None)
        };
        let closure = b.closure.take();
        let mut merged = ops::merge([b]);
        merged.closure = closure;
        merged
    };
    report.assemble_secs = secs(t);
    report.total_secs = secs(run_started);
    tracing::info!(secs = format!("{:.1}", report.assemble_secs), "bundle assembled");
    Ok((bundle, report))
}

async fn fetch_tar<'a>(
    fetcher: &Fetcher,
    loc: &ReleaseLocation,
    tar: &'a mut Option<bytes::Bytes>,
) -> Result<&'a bytes::Bytes> {
    if tar.is_none() {
        *tar = Some(fetcher.get(&loc.file("nixexprs.tar.xz")).await?);
    }
    Ok(tar.as_ref().expect("set above"))
}

/// The release's nixexprs tree, unpacked into a temporary directory once.
async fn unpack<'a>(
    fetcher: &Fetcher,
    loc: &ReleaseLocation,
    tar: &mut Option<bytes::Bytes>,
    unpacked: &'a mut Option<(tempfile::TempDir, std::path::PathBuf)>,
) -> Result<&'a std::path::Path> {
    if unpacked.is_none() {
        let tar = fetch_tar(fetcher, loc, tar).await?;
        let dir = tempfile::tempdir()?;
        let top = tarball::extract(tar, dir.path())?;
        *unpacked = Some((dir, top));
    }
    Ok(&unpacked.as_ref().expect("set above").1)
}

/// Check that a release directory exists (has a git-revision file).
pub async fn release_exists(fetcher: &Fetcher, loc: &ReleaseLocation) -> Result<bool> {
    Ok(matches!(
        fetcher.get_opt(&loc.file("git-revision")).await?,
        Fetched::Found(_)
    ))
}
