//! Orchestrates indexing of one or more releases into a bundle.

use crate::closure::{self, ClosureOptions};
use crate::fetch::{Fetched, Fetcher};
use crate::outpaths::{self, OutpathsOptions};
use crate::release::ReleaseLocation;
use crate::{packages, programs, tarball};
use anyhow::{Context, Result};
use nfi_interchange::*;

#[derive(Clone, Debug)]
pub struct IndexOptions {
    pub datasets: Vec<Dataset>,
    pub closure: ClosureOptions,
    pub outpaths: OutpathsOptions,
    /// Keep the seed bundle's releases in the output.
    pub include_seed: bool,
}

/// Index `locations` (in order) into a new bundle. When `seed` is given its
/// closure section is used to avoid re-fetching known narinfos.
pub async fn index_releases(
    fetcher: &Fetcher,
    locations: &[ReleaseLocation],
    opts: &IndexOptions,
    seed: Option<Bundle>,
) -> Result<Bundle> {
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

    let mut package_entries = Vec::new();
    let mut outpath_entries = Vec::new();
    let mut program_entries = Vec::new();
    let mut new_releases = Vec::new();

    for loc in locations {
        let mut release = loc.resolve(fetcher).await?;
        tracing::info!(release = %release.name, commit = %release.commit, "indexing release");
        release.datasets = Vec::new();
        let idx = bundle.ensure_release(release.clone());
        new_releases.push(release.name.clone());

        if opts.datasets.contains(&Dataset::Closure) {
            let xz = fetcher.get(&loc.file("store-paths.xz")).await?;
            let mut roots = closure::parse_store_paths(&xz)?;
            if let Some(n) = opts.closure.sample {
                roots.truncate(n);
            }
            let report = closure::index_closure(fetcher, &opts.closure, &mut builder, &roots, idx).await?;
            tracing::info!(
                release = %release.name,
                roots = report.roots,
                fetched = report.fetched,
                reused = report.reused,
                missing = report.missing,
                "closure indexed"
            );
            bundle.releases[idx].add_dataset(Dataset::Closure);
        }
        if opts.datasets.contains(&Dataset::Packages) {
            let br = fetcher.get(&loc.file("packages.json.br")).await?;
            let entries = packages::parse_packages_json_br(&br, idx)?;
            tracing::info!(release = %release.name, packages = entries.len(), "packages indexed");
            package_entries.extend(entries);
            bundle.releases[idx].add_dataset(Dataset::Packages);
        }
        let want_tar =
            opts.datasets.contains(&Dataset::Programs) || opts.datasets.contains(&Dataset::Outpaths);
        if want_tar {
            let tar = fetcher.get(&loc.file("nixexprs.tar.xz")).await?;
            if opts.datasets.contains(&Dataset::Programs) {
                match tarball::read_top_level_file(&tar, "programs.sqlite")? {
                    Some(db) => {
                        let entries = programs::read_programs_db(&db, idx)?;
                        tracing::info!(release = %release.name, programs = entries.len(), "programs indexed");
                        program_entries.extend(entries);
                    }
                    None => tracing::warn!(release = %release.name, "no programs.sqlite in nixexprs tarball"),
                }
                bundle.releases[idx].add_dataset(Dataset::Programs);
            }
            if opts.datasets.contains(&Dataset::Outpaths) {
                let dir = tempfile::tempdir()?;
                let top = tarball::extract(&tar, dir.path())?;
                let entries = outpaths::evaluate(&top, &opts.outpaths, idx)
                    .await
                    .with_context(|| format!("evaluating {}", release.name))?;
                tracing::info!(release = %release.name, outpaths = entries.len(), "out paths indexed");
                outpath_entries.extend(entries);
                bundle.releases[idx].add_dataset(Dataset::Outpaths);
            }
        }
    }

    if opts.datasets.contains(&Dataset::Closure)
        || seed_sections.as_ref().is_some_and(|s| s.closure.is_some())
    {
        bundle.closure = Some(builder.finish());
    }
    if opts.datasets.contains(&Dataset::Packages) {
        bundle.packages = Some(PackagesSection {
            entries: package_entries,
        });
    }
    if opts.datasets.contains(&Dataset::Outpaths) {
        bundle.outpaths = Some(OutpathsSection {
            entries: outpath_entries,
        });
    }
    if opts.datasets.contains(&Dataset::Programs) {
        bundle.programs = Some(ProgramsSection {
            entries: program_entries,
        });
    }

    let bundle = if let (true, Some(seed)) = (opts.include_seed, seed_sections) {
        // Bring along the seed's non-closure sections too.
        let seed_rest = Bundle {
            closure: None,
            ..seed
        };
        ops::merge([bundle, seed_rest])
    } else if !seed_releases.is_empty() {
        ops::merge([ops::filter(&bundle, |r| new_releases.contains(&r.name), None)])
    } else {
        // Merging a single bundle deduplicates entries across its releases.
        ops::merge([bundle])
    };
    Ok(bundle)
}

/// Check that a release directory exists (has a git-revision file).
pub async fn release_exists(fetcher: &Fetcher, loc: &ReleaseLocation) -> Result<bool> {
    Ok(matches!(
        fetcher.get_opt(&loc.file("git-revision")).await?,
        Fetched::Found(_)
    ))
}
