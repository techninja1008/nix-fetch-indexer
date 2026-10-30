use anyhow::{Result, bail};
use clap::{Args, Parser, Subcommand};
use nfi::*;
use nfi_api::client::Client;
use nfi_api::drv::ReducedGraph;
use nfi_api::{BulkRequest, ItemStatus, SearchQuery};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser)]
#[command(name = "nfi", version, about = "Query the nix-fetch-indexer service")]
struct Cli {
    /// Service URL [default: config `url`, or http://localhost:8080]
    #[arg(long, env = "NFI_URL", global = true)]
    url: Option<String>,
    /// Configuration file [default: $XDG_CONFIG_HOME/nfi/config.toml]
    #[arg(long, env = "NFI_CONFIG", global = true)]
    config: Option<PathBuf>,
    /// Read the bearer token from this file instead of using `nfi login`
    /// (e.g. a CI-issued ID token) [also: $NFI_TOKEN]
    #[arg(long, env = "NFI_TOKEN_FILE", global = true)]
    token_file: Option<PathBuf>,
    /// Print raw JSON responses.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Args, Clone)]
struct HookArgs {
    /// Command run with a file listing the resolved store paths; `{}` is
    /// replaced by the file path (appended if absent) [env: NFI_FETCH_HOOK]
    #[arg(long)]
    fetch_hook: Option<String>,
    /// Do not run the fetch hook even if one is configured.
    #[arg(long)]
    no_fetch_hook: bool,
}

#[derive(Subcommand, Clone)]
enum Cmd {
    /// Log in to the service with OIDC (device code flow).
    Login {
        /// Log in again even if a cached login is still valid.
        #[arg(long)]
        force: bool,
    },
    /// Forget the cached login for the service.
    Logout,
    /// Show who the service thinks you are.
    Whoami,
    /// Full-text search over packages and programs.
    Search {
        query: Vec<String>,
        #[arg(long)]
        attr: Option<String>,
        #[arg(long)]
        pname: Option<String>,
        #[arg(long)]
        version: Option<String>,
        /// Program (binary) name.
        #[arg(long)]
        program: Option<String>,
        #[arg(long)]
        system: Option<String>,
        /// Release name.
        #[arg(long)]
        release: Option<String>,
        /// Channel, e.g. nixos-25.05.
        #[arg(long)]
        channel: Option<String>,
        /// Release series, e.g. 25.05 or unstable.
        #[arg(long)]
        series: Option<String>,
        #[arg(long)]
        commit: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: usize,
        #[arg(long, default_value_t = 0)]
        offset: usize,
    },
    /// List the indexed releases with their datasets, and their evaluations
    /// with their closure types and error counts.
    Releases {
        /// Only releases in this channel, e.g. nixos-25.05.
        #[arg(long)]
        channel: Option<String>,
        /// Also list the attributes that failed to evaluate, not only their
        /// count.
        #[arg(long)]
        errors: bool,
    },
    /// Show a package's metadata and output paths at a nixpkgs commit.
    Show {
        attr: String,
        #[arg(long)]
        commit: String,
        #[arg(long)]
        system: Option<String>,
    },
    /// Resolve many attr@commit pairs to store paths and their closure.
    Bulk {
        /// Items as attr@commit[@system].
        items: Vec<String>,
        /// Read items from a file (`-` for stdin): JSON or one item per line.
        #[arg(short = 'f', long)]
        file: Option<String>,
        /// Default system for items without one.
        #[arg(long)]
        system: Option<String>,
        /// Outputs to select (`*` for all) [default: out]
        #[arg(long, value_delimiter = ',')]
        outputs: Option<Vec<String>>,
        /// Exit non-zero if any item could not be fully resolved.
        #[arg(long)]
        strict: bool,
        #[command(flatten)]
        hook: HookArgs,
    },
    /// Find the minimal set of cached paths useful for building installables.
    Minimise {
        /// Flake refs, .drv paths or other installables.
        installables: Vec<String>,
        /// Read `nix derivation show --recursive` JSON (or a reduced graph)
        /// from a file, `-` for stdin. May be repeated.
        #[arg(short = 'f', long = "file")]
        files: Vec<String>,
        /// nix command used to evaluate installables [default: config `nix`, or nix]
        #[arg(long, env = "NFI_NIX")]
        nix: Option<String>,
        /// Pass only the root paths (not their whole closure) to the fetch hook.
        #[arg(long)]
        roots_only: bool,
        /// Warn about derivations to build whose name matches fewer than this
        /// many indexed paths (likely overrides or unindexed build inputs);
        /// 0 disables [server default: 100].
        #[arg(long)]
        close_miss_threshold: Option<u64>,
        #[command(flatten)]
        hook: HookArgs,
    },
}

fn hook_cmd(args: &HookArgs, cfg: &Config) -> Option<String> {
    if args.no_fetch_hook {
        return None;
    }
    args.fetch_hook
        .clone()
        .or_else(|| std::env::var("NFI_FETCH_HOOK").ok().filter(|s| !s.is_empty()))
        .or_else(|| cfg.fetch_hook.clone())
}

fn human_size(n: u64) -> String {
    let units = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < units.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    format!("{v:.1} {}", units[u])
}

async fn run(cli: Cli) -> Result<ExitCode> {
    let cfg = load_config(cli.config.as_deref())?;
    let url = cli
        .url
        .clone()
        .or(cfg.url.clone())
        .unwrap_or_else(|| "http://localhost:8080".into());
    let token = nfi_auth::client::explicit(cli.token_file.as_deref(), "NFI_TOKEN")?;
    let login = auth::Login::new("nfi", "set NFI_TOKEN or --token-file", &url, token)?;
    match &cli.cmd {
        Cmd::Login { force } => {
            let w = login.login(*force).await?;
            eprintln!("logged in to {url} as {}", auth::describe(&w));
            return Ok(ExitCode::SUCCESS);
        }
        Cmd::Logout => {
            if login.logout().await? {
                eprintln!("logged out of {url}");
            } else {
                eprintln!("not logged in to {url}");
            }
            return Ok(ExitCode::SUCCESS);
        }
        Cmd::Whoami => {
            let w = login.whoami().await?;
            if cli.json {
                print_json(&w)?;
            } else {
                println!("{}", auth::describe(&w));
            }
            return Ok(ExitCode::SUCCESS);
        }
        _ => {}
    }
    let input = gather(&cli.cmd, &cfg)?;
    login
        .run(|client| exec(cli.cmd.clone(), &input, cli.json, &cfg, client))
        .await
}

/// A command's local input, gathered before any request so that a retry
/// after logging in neither re-reads stdin nor re-runs nix.
enum Input {
    None,
    Bulk(BulkRequest),
    Graph(ReducedGraph),
}

fn gather(cmd: &Cmd, cfg: &Config) -> Result<Input> {
    match cmd {
        Cmd::Bulk {
            items,
            file,
            system,
            outputs,
            ..
        } => {
            let system = system.clone().or(cfg.system.clone());
            let mut all = Vec::new();
            for i in items {
                all.push(parse_item(i, system.as_deref(), outputs)?);
            }
            if let Some(f) = file {
                all.extend(read_items(f, system.as_deref(), outputs)?);
            }
            if all.is_empty() {
                bail!("no items given");
            }
            Ok(Input::Bulk(BulkRequest { items: all }))
        }
        Cmd::Minimise {
            installables,
            files,
            nix,
            ..
        } => {
            if installables.is_empty() && files.is_empty() {
                bail!("give installables and/or --file");
            }
            let mut graph = ReducedGraph::default();
            if !installables.is_empty() {
                let nix = nix.clone().or(cfg.nix.clone()).unwrap_or_else(|| "nix".into());
                graph.merge(derivation_graph(&nix, installables)?);
            }
            for f in files {
                graph.merge(read_graph(f)?);
            }
            Ok(Input::Graph(graph))
        }
        _ => Ok(Input::None),
    }
}

async fn exec(cmd: Cmd, input: &Input, json: bool, cfg: &Config, client: Client) -> Result<ExitCode> {
    match cmd {
        Cmd::Login { .. } | Cmd::Logout | Cmd::Whoami => unreachable!("handled in run"),
        Cmd::Search {
            query,
            attr,
            pname,
            version,
            program,
            system,
            release,
            channel,
            series,
            commit,
            limit,
            offset,
        } => {
            let q = SearchQuery {
                q: Some(query.join(" ")).filter(|s| !s.is_empty()),
                attr,
                pname,
                version,
                program,
                system,
                release,
                channel,
                series,
                commit,
                limit: Some(limit),
                offset: Some(offset),
            };
            let r = client.search(&q).await?;
            if json {
                print_json(&r)?;
            } else {
                for h in &r.hits {
                    println!(
                        "{} {} ({}){}",
                        h.attr,
                        h.version.as_deref().unwrap_or("-"),
                        h.system,
                        h.description
                            .as_deref()
                            .map(|d| format!(": {d}"))
                            .unwrap_or_default()
                    );
                    if !h.programs.is_empty() {
                        println!("    programs: {}", h.programs.join(" "));
                    }
                    println!("    releases: {}", h.releases.join(" "));
                }
                eprintln!("{} of ~{} results", r.hits.len(), r.estimated_total);
            }
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Releases { channel, errors } => {
            let rels = client.releases(channel.as_deref(), errors).await?;
            if json {
                print_json(&rels)?;
            } else {
                print!("{}", nfi_api::releases_table(&rels));
            }
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Show { attr, commit, system } => {
            let system = system.or(cfg.system.clone());
            let p = client.package(&attr, &commit, system.as_deref()).await?;
            if json {
                print_json(&p)?;
            } else {
                println!("{} ({})", p.attr, p.system);
                println!(
                    "  release:     {} [{}] {}",
                    p.release.name, p.release.channel, p.release.commit
                );
                if let Some(n) = &p.name {
                    println!("  name:        {n}");
                }
                if let Some(d) = &p.description {
                    println!("  description: {d}");
                }
                if let Some(h) = &p.homepage {
                    println!("  homepage:    {h}");
                }
                if !p.license.is_empty() {
                    println!("  license:     {}", p.license.join(", "));
                }
                if !p.programs.is_empty() {
                    println!("  programs:    {}", p.programs.join(" "));
                }
                if let Some(d) = &p.drv_path {
                    println!("  drv:         {d}");
                }
                for (o, i) in &p.outputs {
                    println!(
                        "  {o:<12} {}{}",
                        i.path,
                        if i.cached { "" } else { "  (not cached)" }
                    );
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Bulk { strict, hook, .. } => {
            let Input::Bulk(req) = input else {
                unreachable!("gathered in run")
            };
            let hook = hook_cmd(&hook, cfg);
            let r = client.bulk(req).await?;
            let failed: Vec<_> = r.items.iter().filter(|i| i.status != ItemStatus::Ok).collect();
            for i in &failed {
                eprintln!("warning: {}@{} ({}): {:?}", i.attr, i.commit, i.system, i.status);
            }
            if json {
                print_json(&r)?;
            } else if hook.is_none() {
                for p in &r.closure {
                    println!("{p}");
                }
            }
            eprintln!(
                "{} paths in closure ({} download, {} unpacked)",
                r.closure.len(),
                human_size(r.file_size),
                human_size(r.nar_size)
            );
            if let Some(h) = hook {
                let code = run_fetch_hook(&h, &r.closure)?;
                if code != 0 {
                    return Ok(ExitCode::from(code.clamp(1, 255) as u8));
                }
            }
            Ok(if strict && !failed.is_empty() {
                ExitCode::from(2)
            } else {
                ExitCode::SUCCESS
            })
        }
        Cmd::Minimise {
            roots_only,
            close_miss_threshold,
            hook,
            ..
        } => {
            let Input::Graph(graph) = input else {
                unreachable!("gathered in run")
            };
            let hook = hook_cmd(&hook, cfg);
            let r = client.minimise(graph, close_miss_threshold).await?;
            let paths = if roots_only { &r.roots } else { &r.closure };
            if json {
                print_json(&r)?;
            } else if hook.is_none() {
                for p in paths {
                    println!("{p}");
                }
            }
            for u in &r.unresolved {
                eprintln!("warning: input derivation not in graph: {u}");
            }
            for w in &r.warnings {
                eprintln!("warning: {}", w.message);
            }
            eprintln!(
                "{} requested, {} to build, {} to fetch; {} root paths, {} in closure ({} download, {} unpacked)",
                r.requested.len(),
                r.to_build.len(),
                r.to_fetch.len(),
                r.roots.len(),
                r.closure.len(),
                human_size(r.file_size),
                human_size(r.nar_size)
            );
            if let Some(h) = hook {
                let code = run_fetch_hook(&h, paths)?;
                if code != 0 {
                    return Ok(ExitCode::from(code.clamp(1, 255) as u8));
                }
            }
            Ok(ExitCode::SUCCESS)
        }
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    match run(Cli::parse()).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}
