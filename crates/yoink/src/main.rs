use anyhow::{Context, Result};
use clap::{ArgAction, Args, Parser, Subcommand};
use nfi_auth::Whoami;
use nfi_auth::client::{Session, TokenStore, explicit};
use std::path::PathBuf;
use std::process::ExitCode;
use yoink::auth::{NFI_HINT, YOINK_HINT, bearer, with_login};
use yoink::pull::{PullOptions, pull};

#[derive(Parser)]
#[command(
    name = "yoink",
    version,
    about = "Pull a flake's cached dependencies into a local binary cache",
    args_conflicts_with_subcommands = true,
    subcommand_negates_reqs = true
)]
struct Cli {
    /// What to pull: flake references with an attribute (nixpkgs#hello,
    /// .#foo), `.` for the current flake's default package, or package URLs
    /// (pkg:nix/%2Fnix%2Fstore%2F<hash>-<name>).
    #[arg(required = true)]
    targets: Vec<String>,
    /// Line output instead of the fullscreen monitor (the default when
    /// stdout is not a terminal).
    #[arg(long)]
    plain: bool,
    /// More output in plain mode (-v: warnings and info, -vv: progress).
    #[arg(short, long, action = ArgAction::Count, conflicts_with = "quiet")]
    verbose: u8,
    /// Only errors and the summary in plain mode.
    #[arg(short, long)]
    quiet: bool,
    /// Passed to NFI minimise: warn about derivations to build whose name
    /// matches fewer than this many indexed paths; 0 disables.
    #[arg(long)]
    close_miss_threshold: Option<u64>,
    #[command(flatten)]
    common: Common,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Args)]
struct Common {
    /// yoink service URL [default: config `url`]
    #[arg(long, env = "YOINK_URL", global = true)]
    url: Option<String>,
    /// nix-fetch-indexer service URL, used for flake references [default: config `nfi_url`]
    #[arg(long, env = "NFI_URL", global = true)]
    nfi_url: Option<String>,
    /// Configuration file [default: $XDG_CONFIG_HOME/yoink/config.toml]
    #[arg(long, env = "YOINK_CONFIG", global = true)]
    config: Option<PathBuf>,
    /// nix command used to evaluate flake references [default: config `nix`, or nix]
    #[arg(long, env = "NFI_NIX", global = true)]
    nix: Option<String>,
    /// Read the yoink service token (an ID token) from a file instead of
    /// using the login cache [env: YOINK_TOKEN holds the token itself]
    #[arg(long, env = "YOINK_TOKEN_FILE", global = true)]
    token_file: Option<PathBuf>,
    /// Read the NFI token from a file [env: NFI_TOKEN holds the token itself]
    #[arg(long, env = "NFI_TOKEN_FILE", global = true)]
    nfi_token_file: Option<PathBuf>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Log in to the yoink service (and the NFI service, if configured and
    /// using another provider or client) with the OIDC device flow.
    Login,
    /// Forget the cached logins for the yoink and NFI services.
    Logout,
    /// Show who the services see you as.
    Whoami,
}

fn prompt(p: &nfi_auth::client::DevicePrompt) {
    eprintln!("{p}");
}

async fn whoami(url: &str, token: Option<String>) -> Result<Whoami> {
    yoink_api::client::Client::new(url, token)?
        .get_json(nfi_auth::WHOAMI_PATH)
        .await
}

async fn run(cli: Cli) -> Result<ExitCode> {
    let cfg = yoink::config::load(cli.common.config.as_deref())?;
    let url = cli.common.url.clone().or(cfg.url.clone());
    let nfi_url = cli.common.nfi_url.clone().or(cfg.nfi_url.clone());
    let token = explicit(cli.common.token_file.as_deref(), "YOINK_TOKEN")?;
    let nfi_token = explicit(cli.common.nfi_token_file.as_deref(), "NFI_TOKEN")?;
    let need_url = || {
        url.clone()
            .context("no yoink service URL: set --url, YOINK_URL or `url` in the config file")
    };
    let session = || -> Result<Session> { Session::new(TokenStore::default_location()?) };
    match cli.cmd {
        Some(Cmd::Login) => {
            let url = need_url()?;
            let s = session()?;
            let (cfg_y, _) = s.login(&url, true, prompt).await?;
            eprintln!("logged in to {url}");
            if let Some(n) = &nfi_url {
                match s.discover(n).await {
                    Ok(Some(cfg_n)) => {
                        // The same provider and client share the login just made.
                        let same = cfg_n.issuer == cfg_y.issuer && cfg_n.client_id == cfg_y.client_id;
                        s.login(n, !same, prompt).await?;
                        eprintln!("logged in to {n}");
                    }
                    Ok(None) => {}
                    Err(e) => eprintln!("warning: not logging in to {n}: {e:#}"),
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Some(Cmd::Logout) => {
            let s = session()?;
            for u in [url.clone(), nfi_url.clone()].into_iter().flatten() {
                if s.logout(&u).await? {
                    eprintln!("logged out of {u}");
                } else {
                    eprintln!("not logged in to {u}");
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Some(Cmd::Whoami) => {
            let s = yoink::auth::session()?;
            let url = need_url()?;
            let mut targets = vec![(url, token, YOINK_HINT)];
            if let Some(n) = nfi_url {
                targets.push((n, nfi_token, NFI_HINT));
            }
            let mut ok = true;
            for (u, t, hint) in targets {
                let explicit = t.is_some();
                let mut t = bearer(s.as_ref(), &u, t).await?;
                let r = with_login(s.as_ref(), &u, &mut t, explicit, hint, |t| {
                    let u = &u;
                    async move { whoami(u, t).await }
                })
                .await;
                match r {
                    Ok(w) => println!("{u}: {}", nfi::auth::describe(&w)),
                    Err(e) => {
                        ok = false;
                        println!("{u}: {e:#}");
                    }
                }
            }
            Ok(if ok { ExitCode::SUCCESS } else { ExitCode::FAILURE })
        }
        None => {
            pull(PullOptions {
                targets: cli.targets,
                url: need_url()?,
                nfi_url,
                nix: cli.common.nix.or(cfg.nix).unwrap_or_else(|| "nix".into()),
                token,
                nfi_token,
                plain: cli.plain,
                verbosity: if cli.quiet { -1 } else { cli.verbose.min(2) as i8 },
                close_miss_threshold: cli.close_miss_threshold,
            })
            .await
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
