use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use nfi::print_json;
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

#[derive(Subcommand, Clone)]
enum Cmd {
    /// Log in to the admin listener with OIDC (device code flow).
    Login {
        /// Log in again even if a cached login is still valid.
        #[arg(long)]
        force: bool,
    },
    /// Forget the cached login for the admin listener.
    Logout,
    /// Show who the admin listener thinks you are.
    Whoami,
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
        /// Also list the attributes that failed to evaluate, not only their
        /// count.
        #[arg(long)]
        errors: bool,
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
    let j = wait_job(client, created.id).await?;
    if j.status != JobStatus::Succeeded {
        print_job(&j, json)?;
        return Ok(Outcome::Failed);
    }
    let report: nfi_api::CheckReport =
        serde_json::from_value(j.result.clone().unwrap_or_default()).context("decoding check result")?;
    if json {
        print_json(&report)?;
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
        return print_json(j);
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

/// Wait for a job, printing its progress.
async fn wait_job(client: &Client, id: i64) -> Result<JobInfo> {
    client
        .wait_job(id, |j| {
            if let Some(p) = &j.progress {
                eprintln!("job {}: {p}", j.id);
            }
        })
        .await
}

async fn wait(client: &Client, id: i64, json: bool) -> Result<bool> {
    let j = wait_job(client, id).await?;
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
    let token = match cli.token.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
        Some(t) => Some(t.to_string()),
        None => cli
            .token_file
            .as_deref()
            .map(nfi_auth::client::read_token_file)
            .transpose()?,
    };
    let login = nfi::auth::Login::new(
        "nfi-admin",
        "pass --token or --token-file (NFI_ADMIN_TOKEN, NFI_ADMIN_TOKEN_FILE)",
        &cli.url,
        token,
    )?;
    match &cli.cmd {
        Cmd::Login { force } => {
            let w = login.login(*force).await?;
            eprintln!("logged in to {} as {}", cli.url, nfi::auth::describe(&w));
            return Ok(Outcome::Ok);
        }
        Cmd::Logout => {
            if login.logout().await? {
                eprintln!("logged out of {}", cli.url);
            } else {
                eprintln!("not logged in to {}", cli.url);
            }
            return Ok(Outcome::Ok);
        }
        Cmd::Whoami => {
            let w = login.whoami().await?;
            if cli.json {
                print_json(&w)?;
            } else {
                println!("{}", nfi::auth::describe(&w));
            }
            return Ok(Outcome::Ok);
        }
        _ => {}
    }
    login.run(|client| exec(cli.cmd.clone(), cli.json, client)).await
}

async fn exec(cmd: Cmd, json: bool, client: Client) -> Result<Outcome> {
    let ok = match cmd {
        Cmd::Login { .. } | Cmd::Logout | Cmd::Whoami => unreachable!("handled in run"),
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
                ok &= submitted(&client, created.id, wait, json).await?;
            }
            Ok(ok)
        }
        Cmd::Jobs {
            id: Some(id),
            wait: true,
            ..
        } => wait(&client, id, json).await,
        Cmd::Jobs { id: Some(id), .. } => {
            print_job(&client.job(id).await?, json)?;
            Ok(true)
        }
        Cmd::Jobs { id: None, limit, .. } => {
            let jobs = client.jobs(limit).await?;
            if json {
                print_json(&jobs)?;
            } else {
                for j in &jobs {
                    print_job(j, false)?;
                }
            }
            Ok(true)
        }
        Cmd::Releases { channel, errors } => {
            let rels = client.admin_releases(channel.as_deref(), errors).await?;
            if json {
                print_json(&rels)?;
            } else {
                print!("{}", nfi_api::releases_table(&rels));
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
            submitted(&client, created.id, wait, json).await
        }
        Cmd::Reindex { wait } => {
            let created = client.reindex().await?;
            submitted(&client, created.id, wait, json).await
        }
        Cmd::Check { wait } => return check_cmd(&client, wait, json).await,
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
