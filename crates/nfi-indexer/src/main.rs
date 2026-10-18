use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use nfi_indexer::closure::{ClosureOptions, DEFAULT_CACHE};
use nfi_indexer::fetch::Fetcher;
use nfi_indexer::index::{IndexOptions, index_releases_with_report};
use nfi_indexer::outpaths::OutpathsOptions;
use nfi_indexer::release::{self, DEFAULT_RELEASES_BASE, DEFAULT_S3_LIST_BASE, ReleaseLocation};
use nfi_interchange::codec::{self, Compression, Format};
use nfi_interchange::{Dataset, ops};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "nfi-indexer",
    version,
    about = "Index NixOS releases into interchange bundles"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Index the binary cache closure of a release (store-paths.xz + narinfos).
    Closure(IndexCmd),
    /// Index packages.json metadata.
    Packages(IndexCmd),
    /// Index evaluated output store paths (runs nix-eval-jobs).
    Outpaths(IndexCmd),
    /// Index programs.sqlite (command-not-found) from the nixexprs tarball.
    Programs(IndexCmd),
    /// Index several datasets in one run (default: closure, packages, programs).
    All(IndexCmd),
    /// List the releases under a channel prefix (e.g. nixos/26.05), oldest
    /// first, as `name<TAB>url` lines.
    ListReleases {
        prefix: String,
        /// Only releases newer than this release name.
        #[arg(long)]
        since: Option<String>,
        #[arg(long, default_value = DEFAULT_RELEASES_BASE, env = "NFI_RELEASES_BASE")]
        releases_base: String,
        #[arg(long, default_value = DEFAULT_S3_LIST_BASE, env = "NFI_S3_LIST_BASE")]
        s3_list_base: String,
    },
    /// List the channels under a release prefix (e.g. nixos), one per line.
    ListChannels {
        #[arg(default_value = "nixos")]
        prefix: String,
        #[arg(long, default_value = DEFAULT_S3_LIST_BASE, env = "NFI_S3_LIST_BASE")]
        s3_list_base: String,
    },
    /// Merge bundles.
    Merge {
        inputs: Vec<String>,
        #[command(flatten)]
        out: OutArgs,
    },
    /// Write one bundle per release into a directory.
    Split {
        input: String,
        #[arg(short = 'o', long)]
        out_dir: PathBuf,
        #[arg(long, default_value = "binary")]
        format: String,
        #[arg(long)]
        compression: Option<String>,
    },
    /// Keep a subset of releases and/or sections.
    Filter {
        input: String,
        /// Release names to keep.
        #[arg(long = "release")]
        releases: Vec<String>,
        /// Commits (or unique prefixes) to keep.
        #[arg(long = "commit")]
        commits: Vec<String>,
        /// Channels to keep (e.g. nixos-25.05).
        #[arg(long = "channel")]
        channels: Vec<String>,
        /// Sections to keep (closure, packages, outpaths, programs).
        #[arg(long = "section", value_delimiter = ',')]
        sections: Vec<Dataset>,
        #[command(flatten)]
        out: OutArgs,
    },
    /// Convert between binary and JSON representations.
    Convert {
        input: String,
        #[command(flatten)]
        out: OutArgs,
    },
    /// Show releases, sections and entry counts.
    Inspect {
        input: String,
        #[arg(long)]
        json: bool,
    },
    /// Compare compression algorithms on a bundle.
    BenchCompression {
        input: String,
        /// Algorithms as name[:level].
        #[arg(
            long,
            value_delimiter = ',',
            default_value = "none,zstd:3,zstd:9,zstd:19,xz:6,xz:9,brotli:9,brotli:11"
        )]
        algos: Vec<String>,
    },
}

#[derive(Args, Clone)]
struct OutArgs {
    /// Output file (`-` for stdout).
    #[arg(short = 'o', long, default_value = "-")]
    output: String,
    /// binary or json
    #[arg(long, default_value = "binary")]
    format: String,
    /// none, zstd[:level], xz[:level], brotli[:level] (binary only; default zstd:9)
    #[arg(long)]
    compression: Option<String>,
}

#[derive(Args, Clone)]
struct IndexCmd {
    /// Full release URL, e.g. https://releases.nixos.org/nixos/25.05/nixos-25.05.813814.ac62194c3917
    #[arg(long = "release-url", required_unless_present = "channel_list")]
    release_urls: Vec<String>,
    /// Index every release under a prefix, e.g. nixos/25.05 (oldest first).
    #[arg(long)]
    channel_list: Option<String>,
    /// With --channel-list: only the newest N releases.
    #[arg(long)]
    limit: Option<usize>,
    /// With --channel-list: only releases newer than this release name.
    #[arg(long)]
    since: Option<String>,
    /// Datasets (for `all`).
    #[arg(long, value_delimiter = ',')]
    datasets: Vec<Dataset>,
    /// Existing bundle whose closure is reused to skip known narinfos.
    #[arg(long)]
    seed: Option<String>,
    /// Keep the seed's releases in the output bundle.
    #[arg(long)]
    include_seed: bool,
    #[arg(long, default_value = DEFAULT_CACHE, env = "NFI_CACHE_URL")]
    cache_url: String,
    #[arg(long, default_value = DEFAULT_RELEASES_BASE, env = "NFI_RELEASES_BASE")]
    releases_base: String,
    #[arg(long, default_value = DEFAULT_S3_LIST_BASE, env = "NFI_S3_LIST_BASE")]
    s3_list_base: String,
    /// Independent HTTP connections to spread narinfo requests over.
    #[arg(long, default_value_t = 8)]
    connections: usize,
    /// Concurrent narinfo requests (spread over --connections).
    #[arg(long, default_value_t = 512)]
    concurrency: usize,
    #[arg(long, default_value_t = 6)]
    retries: u32,
    /// Only index the first N store paths of store-paths.xz (testing).
    #[arg(long)]
    sample: Option<usize>,
    /// Systems to evaluate out paths for.
    #[arg(long, value_delimiter = ',', default_value = "x86_64-linux")]
    systems: Vec<String>,
    /// nix-eval-jobs command (shell words).
    #[arg(long, default_value = "nix-eval-jobs", env = "NFI_NIX_EVAL_JOBS")]
    nix_eval_jobs: String,
    #[arg(long, default_value_t = 4)]
    eval_workers: usize,
    #[arg(long, default_value_t = 4096)]
    eval_max_memory_mb: usize,
    #[command(flatten)]
    out: OutArgs,
}

fn format_of(out: &OutArgs) -> Result<Format> {
    Format::parse(&out.format, out.compression.as_deref())
}

fn mib(bytes: u64) -> String {
    format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
}

fn format_label(format: Format) -> String {
    match format {
        Format::Json => "json".into(),
        Format::Binary(Compression::None) => "binary (uncompressed)".into(),
        Format::Binary(Compression::Zstd(l)) => format!("binary (zstd:{l})"),
        Format::Binary(Compression::Xz(l)) => format!("binary (xz:{l})"),
        Format::Binary(Compression::Brotli(l)) => format!("binary (brotli:{l})"),
    }
}

/// Write the bundle, logging what is written and how long it took.
fn write_out(bundle: &nfi_interchange::Bundle, out: &OutArgs) -> Result<codec::WriteStats> {
    let format = format_of(out)?;
    let dest = if out.output == "-" {
        "<stdout>"
    } else {
        out.output.as_str()
    };
    let st = ops::stats(bundle);
    tracing::info!(
        output = dest,
        format = %format_label(format),
        releases = st.releases.len(),
        closure_paths = st.closure_paths,
        closure_edges = st.closure_edges,
        packages = st.packages,
        outpaths = st.outpaths,
        programs = st.programs,
        "writing bundle"
    );
    let t = std::time::Instant::now();
    let stats = if out.output == "-" {
        codec::write(bundle, std::io::stdout().lock(), format)?
    } else {
        codec::write_file(bundle, std::path::Path::new(&out.output), format)?
    };
    let secs = t.elapsed().as_secs_f64();
    tracing::info!(
        output = dest,
        encoded = %mib(stats.encoded_bytes),
        written = %mib(stats.written_bytes),
        ratio = format!("{:.2}", stats.encoded_bytes as f64 / stats.written_bytes.max(1) as f64),
        secs = format!("{secs:.1}"),
        throughput = format!("{}/s", mib((stats.encoded_bytes as f64 / secs.max(1e-9)) as u64)),
        "bundle written"
    );
    Ok(stats)
}

/// Final summary of an indexing run.
fn log_summary(
    report: &nfi_indexer::index::IndexReport,
    bundle: &nfi_interchange::Bundle,
    out: &OutArgs,
    written: &codec::WriteStats,
    write_secs: f64,
    total_secs: f64,
) {
    let st = ops::stats(bundle);
    for r in &report.releases {
        let mut parts = Vec::new();
        if let Some(c) = &r.closure {
            parts.push(format!(
                "closure: {} roots, {} narinfos fetched, {} reused, {} missing in {:.1}s",
                c.roots, c.fetched, c.reused, c.missing, r.closure_secs
            ));
        }
        if let Some(n) = r.packages {
            parts.push(format!("packages: {n} in {:.1}s", r.packages_secs));
        }
        if let Some(n) = r.programs {
            parts.push(format!("programs: {n} in {:.1}s", r.programs_secs));
        }
        if let Some(n) = r.outpaths {
            parts.push(format!("outpaths: {n} in {:.1}s", r.outpaths_secs));
        }
        tracing::info!(release = %r.name, commit = %r.commit, "summary: {}", parts.join("; "));
    }
    let fetched: usize = report
        .releases
        .iter()
        .filter_map(|r| r.closure.as_ref())
        .map(|c| c.fetched)
        .sum();
    tracing::info!(
        releases = st.releases.len(),
        closure_paths = st.closure_paths,
        closure_edges = st.closure_edges,
        missing = st.missing_paths,
        packages = st.packages,
        outpaths = st.outpaths,
        programs = st.programs,
        narinfos_fetched = fetched,
        output = if out.output == "-" { "<stdout>" } else { out.output.as_str() },
        size = %mib(written.written_bytes),
        index_secs = format!("{:.1}", report.total_secs),
        write_secs = format!("{write_secs:.1}"),
        total_secs = format!("{total_secs:.1}"),
        "summary: done"
    );
}

async fn run_index(cmd: IndexCmd, default: &[Dataset]) -> Result<()> {
    let started = std::time::Instant::now();
    let datasets = if cmd.datasets.is_empty() {
        default.to_vec()
    } else {
        cmd.datasets.clone()
    };
    let fetcher = Fetcher::with_connections(cmd.retries, cmd.connections)?;
    let mut locations: Vec<ReleaseLocation> = cmd
        .release_urls
        .iter()
        .map(|u| ReleaseLocation::parse(u))
        .collect::<Result<_>>()?;
    if let Some(prefix) = &cmd.channel_list {
        let mut listed =
            release::list_channel(&fetcher, &cmd.s3_list_base, &cmd.releases_base, prefix).await?;
        if let Some(since) = &cmd.since {
            let floor = release::revcount(since);
            listed.retain(|l| release::revcount(&l.name) > floor);
        }
        if let Some(n) = cmd.limit {
            let skip = listed.len().saturating_sub(n);
            listed.drain(..skip);
        }
        tracing::info!(count = listed.len(), %prefix, "releases listed");
        locations.extend(listed);
    }
    if locations.is_empty() {
        bail!("no releases to index");
    }
    let seed = cmd
        .seed
        .as_deref()
        .map(codec::read_path_or_stdin)
        .transpose()
        .context("reading seed")?;
    let opts = IndexOptions {
        datasets,
        closure: ClosureOptions {
            cache_url: cmd.cache_url.clone(),
            concurrency: cmd.concurrency,
            sample: cmd.sample,
        },
        outpaths: OutpathsOptions {
            command: shell_split(&cmd.nix_eval_jobs)?,
            systems: cmd.systems.clone(),
            workers: cmd.eval_workers,
            max_memory_mb: cmd.eval_max_memory_mb,
        },
        include_seed: cmd.include_seed,
    };
    let (bundle, report) = index_releases_with_report(&fetcher, &locations, &opts, seed).await?;
    bundle.validate().map_err(anyhow::Error::msg)?;
    let t = std::time::Instant::now();
    let written = write_out(&bundle, &cmd.out)?;
    log_summary(
        &report,
        &bundle,
        &cmd.out,
        &written,
        t.elapsed().as_secs_f64(),
        started.elapsed().as_secs_f64(),
    );
    Ok(())
}

fn shell_split(s: &str) -> Result<Vec<String>> {
    let words: Vec<String> = s.split_whitespace().map(str::to_string).collect();
    if words.is_empty() {
        bail!("empty command");
    }
    Ok(words)
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Closure(c) => run_index(c, &[Dataset::Closure]).await,
        Cmd::Packages(c) => run_index(c, &[Dataset::Packages]).await,
        Cmd::Outpaths(c) => run_index(c, &[Dataset::Outpaths]).await,
        Cmd::Programs(c) => run_index(c, &[Dataset::Programs]).await,
        Cmd::All(c) => run_index(c, &[Dataset::Closure, Dataset::Packages, Dataset::Programs]).await,
        Cmd::ListReleases {
            prefix,
            since,
            releases_base,
            s3_list_base,
        } => {
            let fetcher = Fetcher::new(6)?;
            let mut listed = release::list_channel(&fetcher, &s3_list_base, &releases_base, &prefix).await?;
            if let Some(since) = &since {
                let floor = release::revcount(since);
                listed.retain(|l| release::revcount(&l.name) > floor);
            }
            for l in listed {
                println!("{}\t{}", l.name, l.url);
            }
            Ok(())
        }
        Cmd::ListChannels { prefix, s3_list_base } => {
            let fetcher = Fetcher::new(6)?;
            for name in release::list_children(&fetcher, &s3_list_base, &prefix).await? {
                println!("{name}");
            }
            Ok(())
        }
        Cmd::Merge { inputs, out } => {
            // Read inputs one at a time so only the merged result and one
            // input are in memory (cumulative files fold in many bundles).
            let mut error = None;
            let n = inputs.len();
            let bundles = inputs.iter().enumerate().map_while(|(i, p)| {
                tracing::info!(input = %p, "merging {}/{n}", i + 1);
                match codec::read_path_or_stdin(p) {
                    Ok(b) => Some(b),
                    Err(e) => {
                        error = Some(e);
                        None
                    }
                }
            });
            let merged = ops::merge(bundles);
            if let Some(e) = error {
                return Err(e);
            }
            merged.validate().map_err(anyhow::Error::msg)?;
            write_out(&merged, &out).map(|_| ())
        }
        Cmd::Split {
            input,
            out_dir,
            format,
            compression,
        } => {
            let b = codec::read_path_or_stdin(&input)?;
            let fmt = Format::parse(&format, compression.as_deref())?;
            std::fs::create_dir_all(&out_dir)?;
            let ext = if matches!(fmt, Format::Json) {
                "json"
            } else {
                "nfib"
            };
            for part in ops::split(&b) {
                let path = out_dir.join(format!("{}.{ext}", part.releases[0].name));
                codec::write_file(&part, &path, fmt)?;
                eprintln!("{}", path.display());
            }
            Ok(())
        }
        Cmd::Filter {
            input,
            releases,
            commits,
            channels,
            sections,
            out,
        } => {
            let b = codec::read_path_or_stdin(&input)?;
            let any = releases.is_empty() && commits.is_empty() && channels.is_empty();
            let f = ops::filter(
                &b,
                |r| {
                    any || releases.contains(&r.name)
                        || channels.contains(&r.channel)
                        || commits.iter().any(|c| r.commit.starts_with(c.as_str()))
                },
                if sections.is_empty() {
                    None
                } else {
                    Some(&sections)
                },
            );
            write_out(&f, &out).map(|_| ())
        }
        Cmd::Convert { input, out } => write_out(&codec::read_path_or_stdin(&input)?, &out).map(|_| ()),
        Cmd::Inspect { input, json } => {
            let s = ops::stats(&codec::read_path_or_stdin(&input)?);
            if json {
                println!("{}", serde_json::to_string_pretty(&s)?);
            } else {
                println!("format version {}", s.format_version);
                println!(
                    "sections: {}",
                    s.sections
                        .iter()
                        .map(|d| d.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                println!(
                    "closure: {} paths, {} edges, {} missing; packages: {}; outpaths: {}; programs: {}",
                    s.closure_paths, s.closure_edges, s.missing_paths, s.packages, s.outpaths, s.programs
                );
                println!("releases ({}):", s.releases.len());
                for r in &s.releases {
                    println!(
                        "  {} [{}] {} datasets={} closure={} missing={} packages={} outpaths={} programs={}",
                        r.name,
                        r.channel,
                        r.commit,
                        r.datasets
                            .iter()
                            .map(|d| d.as_str())
                            .collect::<Vec<_>>()
                            .join(","),
                        r.closure_paths,
                        r.missing_paths,
                        r.packages,
                        r.outpaths,
                        r.programs
                    );
                }
            }
            Ok(())
        }
        Cmd::BenchCompression { input, algos } => {
            let b = codec::read_path_or_stdin(&input)?;
            let algos = algos
                .iter()
                .map(|a| Compression::parse(a))
                .collect::<Result<Vec<_>>>()?;
            println!(
                "{:<12} {:>14} {:>12} {:>12}",
                "algorithm", "bytes", "encode", "decode"
            );
            for r in codec::bench(&b, &algos)? {
                let label = match r.compression {
                    Compression::None => "none".to_string(),
                    Compression::Zstd(l) => format!("zstd:{l}"),
                    Compression::Xz(l) => format!("xz:{l}"),
                    Compression::Brotli(l) => format!("brotli:{l}"),
                };
                println!(
                    "{label:<12} {:>14} {:>12.2?} {:>12.2?}",
                    r.bytes, r.encode, r.decode
                );
            }
            Ok(())
        }
    }
}
