//! Orchestrates indexing of one or more releases into a bundle.

use crate::closure::{self, ClosureOptions};
use crate::fetch::{Fetched, Fetcher};
use crate::outpaths::{self, OutpathsOptions};
use crate::probes::{self, DIRECT, FULL, HELPERS, ProbeOptions};
use crate::release::ReleaseLocation;
use crate::{packages, programs, tarball};
use anyhow::{Context, Result};
use nfi_interchange::*;

/// The evaluators and the closure types each produces.
pub const EVALUATORS: &[(&str, &[&str])] = &[(STORE_PATHS, &[""]), (HELPERS, &[DIRECT, FULL])];

/// Something an indexing run produces: the closures of one closure type
/// (`store-paths`, `helpers:direct`, `helpers:full`), or one of the datasets
/// that are read rather than evaluated.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Closures(ClosureType),
    Packages,
    Programs,
    Outpaths,
}

impl std::str::FromStr for Target {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        Ok(match s {
            "packages" => Target::Packages,
            "programs" => Target::Programs,
            "outpaths" => Target::Outpaths,
            _ => {
                let ct: ClosureType = s.parse()?;
                if !EVALUATORS
                    .iter()
                    .any(|(e, names)| *e == ct.eval_type && names.contains(&ct.name.as_str()))
                {
                    let known: Vec<String> = EVALUATORS
                        .iter()
                        .flat_map(|(e, names)| names.iter().map(|n| ClosureType::new(e, n).to_string()))
                        .collect();
                    return Err(format!(
                        "unknown target {s:?} (expected packages, programs, outpaths or a closure \
                         type: {})",
                        known.join(", ")
                    ));
                }
                Target::Closures(ct)
            }
        })
    }
}

impl std::fmt::Display for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Target::Closures(ct) => ct.fmt(f),
            Target::Packages => f.write_str("packages"),
            Target::Programs => f.write_str("programs"),
            Target::Outpaths => f.write_str("outpaths"),
        }
    }
}

/// The closure types `targets` select, in evaluator order.
pub fn closure_types(targets: &[Target]) -> Vec<ClosureType> {
    EVALUATORS
        .iter()
        .flat_map(|(e, names)| names.iter().map(|n| ClosureType::new(e, n)))
        .filter(|ct| {
            targets
                .iter()
                .any(|t| matches!(t, Target::Closures(c) if c == ct))
        })
        .collect()
}

#[derive(Clone, Debug)]
pub struct IndexOptions {
    pub targets: Vec<Target>,
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
    let closure_types = closure_types(&opts.targets);
    let want = |t: Target| opts.targets.contains(&t);
    // Helper probes are evaluated once per release, for all of their closure
    // types; their eval errors are recorded with the evaluation.
    let run_probes = closure_types.iter().any(|ct| ct.eval_type == HELPERS);
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
            let emap: Vec<usize> = seed
                .evals
                .iter()
                .map(|e| bundle.ensure_eval(map[e.release as usize] as u32, &e.eval_type))
                .collect();
            builder.extend_section(c, &|i| emap.get(i).copied());
        }
    }
    let seed_sections = seed;

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

        let probe_roots = if run_probes {
            let dir = unpack(fetcher, loc, &mut tar, &mut unpacked).await?;
            Some(
                probes::run_probes(dir, &opts.probes)
                    .await
                    .with_context(|| format!("running helper probes for {}", release.name))?,
            )
        } else {
            None
        };

        if !closure_types.is_empty() {
            let t = std::time::Instant::now();
            for ct in &closure_types {
                let roots = match ct.eval_type.as_str() {
                    STORE_PATHS => {
                        let xz = fetcher.get(&loc.file("store-paths.xz")).await?;
                        let mut roots = closure::parse_store_paths(&xz)?;
                        if let Some(n) = opts.closure.sample {
                            roots.truncate(n);
                        }
                        roots
                    }
                    HELPERS => {
                        let pr = probe_roots.as_ref().expect("probes run for helper closures");
                        let set = if ct.name == DIRECT { &pr.direct } else { &pr.full };
                        set.iter().cloned().collect()
                    }
                    other => unreachable!("closure types come from EVALUATORS: {other}"),
                };
                let e = bundle.ensure_eval(idx as u32, &ct.eval_type);
                let cidx = builder.ensure_closure(e, &ct.name);
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
                rr.closures.push((ct.to_string(), report));
            }
            rr.closure_secs = secs(t);
            bundle.releases[idx].add_dataset(Dataset::Closure);
        }
        if let Some(pr) = &probe_roots {
            let e = bundle.ensure_eval(idx as u32, HELPERS);
            eval_errors.evals.set(e);
            for (attr, system) in &pr.errors {
                eval_errors.add_error(attr, system, e);
            }
            rr.eval_errors = Some(pr.errors.len());
            bundle.releases[idx].add_dataset(Dataset::EvalErrors);
        }
        if want(Target::Packages) {
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
            if want(Target::Programs) {
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
            if want(Target::Outpaths) {
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

    if !closure_types.is_empty() || seed_sections.as_ref().is_some_and(|s| s.closure.is_some()) {
        bundle.closure = Some(builder.finish());
    }
    if want(Target::Packages) {
        bundle.packages = Some(PackagesSection {
            entries: package_entries.into_entries(),
        });
    }
    if want(Target::Outpaths) {
        bundle.outpaths = Some(OutpathsSection {
            entries: outpath_entries.into_entries(),
        });
    }
    if want(Target::Programs) {
        bundle.programs = Some(ProgramsSection {
            entries: program_entries.into_entries(),
        });
    }
    if run_probes {
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
        // section); a single-input merge keeps the release table order, but
        // may reorder evaluations, so its closures are pointed at theirs.
        let mut b = if seed_releases.is_empty() {
            bundle
        } else {
            ops::filter(&bundle, |r| new_releases.contains(&r.name), None, None, None)
        };
        let mut closure = b.closure.take();
        let (releases, evals) = (b.releases.clone(), b.evals.clone());
        let mut merged = ops::merge([b]);
        for cl in closure.iter_mut().flat_map(|c| &mut c.closures) {
            let e = &evals[cl.eval as usize];
            let r = merged
                .release_index(&releases[e.release as usize].name)
                .expect("merge keeps releases");
            cl.eval = merged
                .eval_index(r as u32, &e.eval_type)
                .expect("merge keeps evaluations") as u32;
        }
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
