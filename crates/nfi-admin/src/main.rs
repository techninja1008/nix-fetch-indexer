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
    /// List releases and the datasets present for each.
    Releases {
        /// Only releases in this channel.
        #[arg(long)]
        channel: Option<String>,
    },
    /// Remove releases by name, commit (prefix) or channel.
    Prune {
        #[arg(long = "release")]
        releases: Vec<String>,
        #[arg(long = "commit")]
        commits: Vec<String>,
        #[arg(long = "channel")]
        channels: Vec<String>,
        #[arg(long)]
        wait: bool,
    },
    /// Rebuild the search index from the database.
    Reindex {
        #[arg(long)]
        wait: bool,
    },
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

async fn run(cli: Cli) -> Result<bool> {
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
    match cli.cmd {
        Cmd::Ingest { sources, wait } => {
            let mut ok = true;
            for s in &sources {
                let created = if s.starts_with("http://") || s.starts_with("https://") {
                    client.ingest_url(s).await?
                } else {
                    client.ingest_file(Path::new(s)).await?
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
                    "{:<40} {:<18} {:<12} {:<42} DATASETS",
                    "RELEASE", "CHANNEL", "SERIES", "COMMIT"
                );
                for r in &rels {
                    println!(
                        "{:<40} {:<18} {:<12} {:<42} {}",
                        r.name,
                        r.channel,
                        r.series,
                        r.commit,
                        r.datasets.join(",")
                    );
                }
            }
            Ok(true)
        }
        Cmd::Prune {
            releases,
            commits,
            channels,
            wait,
        } => {
            let req = PruneRequest {
                releases,
                commits,
                channels,
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
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    match run(Cli::parse()).await {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(2),
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}
