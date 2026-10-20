use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use nfi_api::client::Client;
use nfi_api::{JobInfo, JobStatus, PruneRequest};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[derive(Parser)]
#[command(
    name = "nfi-admin",
    version,
    about = "Administer the nix-fetch-indexer service"
)]
struct Cli {
    /// Admin listener URL.
    #[arg(
        long,
        env = "NFI_ADMIN_URL",
        default_value = "http://127.0.0.1:8081",
        global = true
    )]
    url: String,
    /// Admin bearer token.
    #[arg(long, env = "NFI_ADMIN_TOKEN", hide_env_values = true, global = true)]
    token: Option<String>,
    /// Read the admin token from a file.
    #[arg(long, env = "NFI_ADMIN_TOKEN_FILE", global = true)]
    token_file: Option<PathBuf>,
    /// Print raw JSON.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Ingest interchange bundles (local files are uploaded; http(s) URLs are fetched by the service).
    Ingest {
        #[arg(required = true)]
        sources: Vec<String>,
        /// Closure type to read version 2 bundles (which predate closure
        /// types) as, `<eval type>[:<closure type>]` (e.g. `helpers:full`)
        /// [service default: store-paths].
        #[arg(long)]
        v2_closure_type: Option<String>,
        /// Wait for each job to finish.
        #[arg(long)]
        wait: bool,
    },
    /// Show one job, or list recent jobs.
    Jobs {
        id: Option<i64>,
        #[arg(long)]
        wait: bool,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// List releases with their datasets, and their evaluations with their
    /// closure types and error counts.
    Releases {
        /// Only releases in this channel.
        #[arg(long)]
        channel: Option<String>,
    },
    /// Remove releases by name, commit (prefix) or channel. With
    /// --closure-type, only remove those closures of the selected releases.
    Prune {
        #[arg(long = "release")]
        releases: Vec<String>,
        #[arg(long = "commit")]
        commits: Vec<String>,
        #[arg(long = "channel")]
        channels: Vec<String>,
        /// Only remove the closures of these closure types (e.g.
        /// `helpers:full`), keeping the releases and their other data.
        #[arg(long = "closure-type", value_delimiter = ',')]
        closure_types: Vec<String>,
        #[arg(long)]
        wait: bool,
    },
    /// Rebuild the search index from the database.
    Reindex {
        #[arg(long)]
        wait: bool,
    },
    /// Check the closure index for inconsistencies (dangling references,
    /// unknown closures, orphaned paths, open closures). With --wait, exits
    /// with status 3 if any are found.
    Check {
        #[arg(long)]
        wait: bool,
    },
}

/// How a command finished, mapped to the exit status.
enum Outcome {
    Ok,
    /// A job failed (exit 2).
    Failed,
    /// The consistency check found problems (exit 3).
    Inconsistent,
}

impl From<bool> for Outcome {
    fn from(ok: bool) -> Self {
        if ok { Outcome::Ok } else { Outcome::Failed }
    }
}

fn print_check(r: &nfi_api::CheckReport) {
    println!(
        "{}: {} store paths, {} references, {} releases, {} closures, {} evals, {} eval errors",
        if r.ok { "ok" } else { "INCONSISTENT" },
        r.store_paths,
        r.references,
        r.releases,
        r.closures,
        r.evals,
        r.eval_errors
    );
    for i in &r.issues {
        println!("  {} ({}): {}", i.kind, i.count, i.description);
        for e in &i.examples {
            println!("    {e}");
        }
        if i.count as usize > i.examples.len() {
            println!("    ... and {} more", i.count as usize - i.examples.len());
        }
    }
}

async fn check_cmd(client: &Client, do_wait: bool, json: bool) -> Result<Outcome> {
    let created = client.check().await?;
    if !do_wait {
        return submitted(client, created.id, false, json)
            .await
            .map(Outcome::from);
    }
    let j = client
        .wait_job(created.id, |j| {
            if let Some(p) = &j.progress {
                eprintln!("job {}: {p}", j.id);
            }
        })
        .await?;
    if j.status != JobStatus::Succeeded {
        print_job(&j, json)?;
        return Ok(Outcome::Failed);
    }
    let report: nfi_api::CheckReport =
        serde_json::from_value(j.result.clone().unwrap_or_default()).context("decoding check result")?;
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_check(&report);
    }
    Ok(if report.ok {
        Outcome::Ok
    } else {
        Outcome::Inconsistent
    })
}

fn print_job(j: &JobInfo, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(j)?);
        return Ok(());
    }
    let status = match j.status {
        JobStatus::Queued => "queued",
        JobStatus::Running => "running",
        JobStatus::Succeeded => "succeeded",
        JobStatus::Failed => "failed",
    };
    println!(
        "job {} [{}] {} (created {})",
        j.id,
        j.kind,
        status,
        j.created_at.format("%Y-%m-%d %H:%M:%S")
    );
    if let Some(p) = &j.progress {
        println!("  progress: {p}");
    }
    if let Some(r) = &j.result {
        println!("  result:   {r}");
    }
    if let Some(e) = &j.error {
        println!("  error:    {e}");
    }
    Ok(())
}

async fn wait(client: &Client, id: i64, json: bool) -> Result<bool> {
    let j = client
        .wait_job(id, |j| {
            if let Some(p) = &j.progress {
                eprintln!("job {}: {p}", j.id);
            }
        })
        .await?;
    print_job(&j, json)?;
    Ok(j.status == JobStatus::Succeeded)
}

async fn submitted(client: &Client, id: i64, do_wait: bool, json: bool) -> Result<bool> {
    if do_wait {
        wait(client, id, json).await
    } else {
        if json {
            println!("{}", serde_json::json!({"id": id}));
        } else {
            println!("queued job {id}");
        }
        Ok(true)
    }
}

async fn run(cli: Cli) -> Result<Outcome> {
    let token = match (&cli.token, &cli.token_file) {
        (Some(t), _) => Some(t.clone()),
        (None, Some(f)) => Some(
            std::fs::read_to_string(f)
                .with_context(|| format!("reading {}", f.display()))?
                .trim()
                .to_string(),
        ),
        (None, None) => None,
    };
    let client = Client::new(&cli.url, token)?;
    if let Cmd::Check { wait } = cli.cmd {
        return check_cmd(&client, wait, cli.json).await;
    }
    let ok = match cli.cmd {
        Cmd::Ingest {
            sources,
            v2_closure_type,
            wait,
        } => {
            let mut ok = true;
            let v2 = v2_closure_type.as_deref();
            for s in &sources {
                let created = if s.starts_with("http://") || s.starts_with("https://") {
                    client.ingest_url(s, v2).await?
                } else {
                    client.ingest_file(Path::new(s), v2).await?
                };
                eprintln!("{s}: job {}", created.id);
                ok &= submitted(&client, created.id, wait, cli.json).await?;
            }
            Ok(ok)
        }
        Cmd::Jobs {
            id: Some(id),
            wait: true,
            ..
        } => wait(&client, id, cli.json).await,
        Cmd::Jobs { id: Some(id), .. } => {
            print_job(&client.job(id).await?, cli.json)?;
            Ok(true)
        }
        Cmd::Jobs { id: None, limit, .. } => {
            let jobs = client.jobs(limit).await?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&jobs)?);
            } else {
                for j in &jobs {
                    print_job(j, false)?;
                }
            }
            Ok(true)
        }
        Cmd::Releases { channel } => {
            let mut rels = client.releases().await?;
            if let Some(c) = &channel {
                rels.retain(|r| &r.channel == c);
            }
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&rels)?);
            } else {
                println!(
                    "{:<40} {:<18} {:<12} {:<42} {:<28} EVALS",
                    "RELEASE", "CHANNEL", "SERIES", "COMMIT", "DATASETS"
                );
                for r in &rels {
                    // e.g. `helpers[direct,full] (2 errors)`, `store-paths`.
                    let evals: Vec<String> = r
                        .evals
                        .iter()
                        .map(|e| {
                            let mut s = e.eval_type.clone();
                            if e.closures.iter().any(|c| !c.is_empty()) {
                                s += &format!("[{}]", e.closures.join(","));
                            }
                            if let Some(errors) = &e.errors {
                                s += &format!(" ({} errors)", errors.len());
                            }
                            s
                        })
                        .collect();
                    println!(
                        "{:<40} {:<18} {:<12} {:<42} {:<28} {}",
                        r.name,
                        r.channel,
                        r.series,
                        r.commit,
                        r.datasets.join(","),
                        evals.join(", ")
                    );
                }
            }
            Ok(true)
        }
        Cmd::Prune {
            releases,
            commits,
            channels,
            closure_types,
            wait,
        } => {
            let req = PruneRequest {
                releases,
                commits,
                channels,
                closure_types,
            };
            if req.is_empty() {
                bail!("specify --release, --commit and/or --channel");
            }
            let created = client.prune(&req).await?;
            submitted(&client, created.id, wait, cli.json).await
        }
        Cmd::Reindex { wait } => {
            let created = client.reindex().await?;
            submitted(&client, created.id, wait, cli.json).await
        }
        Cmd::Check { .. } => unreachable!("handled above"),
    }?;
    Ok(ok.into())
}

#[tokio::main]
async fn main() -> ExitCode {
    match run(Cli::parse()).await {
        Ok(Outcome::Ok) => ExitCode::SUCCESS,
        Ok(Outcome::Failed) => ExitCode::from(2),
        Ok(Outcome::Inconsistent) => ExitCode::from(3),
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}
