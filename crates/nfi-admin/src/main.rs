use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use nfi::client::Client;
use nfi::{print_json, whoami};
use nfi_api::{JobInfo, JobStatus, PruneRequest};
use nfi_auth_client::Credentials;
use nfi_auth_client::http::retry;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

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
    /// Read the admin token (the static admin token or an ID token) from a
    /// file instead of using the login; takes precedence over
    /// $NFI_ADMIN_TOKEN, which holds the token itself.
    #[arg(long, env = "NFI_ADMIN_TOKEN_FILE", global = true)]
    token_file: Option<PathBuf>,
    /// Allow sending tokens and logging in over plain http:// to hosts
    /// other than loopback
    #[arg(long, env = "NFI_ALLOW_INSECURE", global = true, value_parser = clap::builder::BoolishValueParser::new())]
    allow_insecure: bool,
    /// Print raw JSON.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Log in to the admin listener with OIDC (device code flow).
    Login {
        /// Log in again even if a cached login is still valid.
        #[arg(long)]
        force: bool,
    },
    /// Forget the cached login for the admin listener (and so for every
    /// other URL logged in with the same OpenID provider and client).
    Logout,
    /// Show who the admin listener sees you as, without logging in (exit 1
    /// when not authenticated).
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
    /// `whoami` found no login (exit 1, like the other CLIs).
    NotAuthenticated,
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

/// The admin API, logging in again when a request gets a 401.
struct Api {
    client: Client,
    creds: Credentials,
}

impl Api {
    /// Make one request with the current token (see [`retry`]).
    async fn call<T, F, Fut>(&self, f: F) -> Result<T>
    where
        F: Fn(Client) -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        retry(&self.creds, |t| f(self.client.with_token(t))).await
    }
}

async fn check_cmd(api: &Api, do_wait: bool, json: bool) -> Result<Outcome> {
    let created = api.call(|c| async move { c.check().await }).await?;
    if !do_wait {
        return submitted(api, created.id, false, json).await.map(Outcome::from);
    }
    let j = wait_job(api, created.id).await?;
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

/// Poll a job until it finishes, printing its progress.
async fn wait_job(api: &Api, id: i64) -> Result<JobInfo> {
    let mut last: Option<String> = None;
    let mut delay = Duration::from_millis(100);
    loop {
        let j = api.call(|c| async move { c.job(id).await }).await?;
        if j.progress != last {
            if let Some(p) = &j.progress {
                eprintln!("job {}: {p}", j.id);
            }
            last = j.progress.clone();
        }
        if j.status.is_finished() {
            return Ok(j);
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(Duration::from_secs(2));
    }
}

async fn wait(api: &Api, id: i64, json: bool) -> Result<bool> {
    let j = wait_job(api, id).await?;
    print_job(&j, json)?;
    Ok(j.status == JobStatus::Succeeded)
}

async fn submitted(api: &Api, id: i64, do_wait: bool, json: bool) -> Result<bool> {
    if do_wait {
        wait(api, id, json).await
    } else {
        if json {
            println!("{}", serde_json::json!({"id": id}));
        } else {
            println!("queued job {id}");
        }
        Ok(true)
    }
}

const TOKEN_HINT: &str = "set NFI_ADMIN_TOKEN or pass --token-file";

async fn run(cli: Cli) -> Result<Outcome> {
    let token = nfi_auth_client::explicit(cli.token_file.as_deref(), "NFI_ADMIN_TOKEN")?;
    let url = &cli.url;
    let creds = Credentials::new("nfi-admin", url, token, TOKEN_HINT).allow_insecure(cli.allow_insecure);
    let ok = match cli.cmd {
        Cmd::Login { force } => {
            let w = creds.login(force).await?;
            eprintln!("logged in to {url} as {}", nfi_auth_client::describe(&w));
            true
        }
        Cmd::Logout => {
            nfi::logout(&creds, url).await?;
            true
        }
        Cmd::Whoami => {
            if !whoami(&creds, url, cli.json).await? {
                return Ok(Outcome::NotAuthenticated);
            }
            true
        }
        cmd => {
            if let Cmd::Prune {
                releases,
                commits,
                channels,
                ..
            } = &cmd
                && releases.is_empty()
                && commits.is_empty()
                && channels.is_empty()
            {
                bail!("specify --release, --commit and/or --channel");
            }
            creds.token(true).await?;
            let api = Api {
                client: Client::new(url, None)?,
                creds,
            };
            return exec(cmd, cli.json, &api).await;
        }
    };
    Ok(ok.into())
}

async fn exec(cmd: Cmd, json: bool, api: &Api) -> Result<Outcome> {
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
                    api.call(|c| async move { c.ingest_url(s, v2).await }).await?
                } else {
                    api.call(|c| async move { c.ingest_file(Path::new(s), v2).await })
                        .await?
                };
                eprintln!("{s}: job {}", created.id);
                ok &= submitted(api, created.id, wait, json).await?;
            }
            Ok(ok)
        }
        Cmd::Jobs {
            id: Some(id),
            wait: true,
            ..
        } => wait(api, id, json).await,
        Cmd::Jobs { id: Some(id), .. } => {
            print_job(&api.call(|c| async move { c.job(id).await }).await?, json)?;
            Ok(true)
        }
        Cmd::Jobs { id: None, limit, .. } => {
            let jobs = api.call(|c| async move { c.jobs(limit).await }).await?;
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
            let channel = channel.as_deref();
            let rels = api
                .call(|c| async move { c.admin_releases(channel, errors).await })
                .await?;
            if json {
                print_json(&rels)?;
            } else {
                print!("{}", nfi::releases_table(&rels));
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
            let req = &PruneRequest {
                releases,
                commits,
                channels,
                closure_types,
            };
            let created = api.call(|c| async move { c.prune(req).await }).await?;
            submitted(api, created.id, wait, json).await
        }
        Cmd::Reindex { wait } => {
            let created = api.call(|c| async move { c.reindex().await }).await?;
            submitted(api, created.id, wait, json).await
        }
        Cmd::Check { wait } => return check_cmd(api, wait, json).await,
    }?;
    Ok(ok.into())
}

#[tokio::main]
async fn main() -> ExitCode {
    match run(Cli::parse()).await {
        Ok(Outcome::Ok) => ExitCode::SUCCESS,
        Ok(Outcome::NotAuthenticated) => ExitCode::FAILURE,
        Ok(Outcome::Failed) => ExitCode::from(2),
        Ok(Outcome::Inconsistent) => ExitCode::from(3),
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}
