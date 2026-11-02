use anyhow::{Context, Result};
use clap::{ArgAction, Args, Parser, Subcommand};
use nfi_auth_client::{Credentials, describe, explicit};
use std::path::PathBuf;
use std::process::ExitCode;
use yoink::pull::{NFI_HINT, PullOptions, YOINK_HINT, pull};

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
    /// using the login; takes precedence over $YOINK_TOKEN, which holds the
    /// token itself
    #[arg(long, env = "YOINK_TOKEN_FILE", global = true)]
    token_file: Option<PathBuf>,
    /// Read the NFI token from a file; takes precedence over $NFI_TOKEN,
    /// which holds the token itself
    #[arg(long, env = "NFI_TOKEN_FILE", global = true)]
    nfi_token_file: Option<PathBuf>,
    /// Allow sending tokens and logging in over plain http:// to hosts
    /// other than loopback
    #[arg(long, env = "NFI_ALLOW_INSECURE", global = true, value_parser = clap::builder::BoolishValueParser::new())]
    allow_insecure: bool,
}

#[derive(Subcommand)]
enum Cmd {
    /// Log in to the yoink service and the NFI service (if configured) with
    /// the OIDC device flow, unless already logged in.
    Login {
        /// Log in again even if a cached login is still valid (once per
        /// OpenID provider and client).
        #[arg(long)]
        force: bool,
    },
    /// Forget the cached logins for the yoink and NFI services (and so for
    /// every other URL logged in with the same OpenID providers and clients).
    Logout,
    /// Show who the services see you as, without logging in (exit 1 when
    /// not authenticated).
    Whoami,
}

async fn run(cli: Cli) -> Result<ExitCode> {
    let cfg = yoink::config::load(cli.common.config.as_deref())?;
    let url = cli.common.url.clone().or(cfg.url.clone());
    let nfi_url = cli.common.nfi_url.clone().or(cfg.nfi_url.clone());
    let token = explicit(cli.common.token_file.as_deref(), "YOINK_TOKEN")?;
    let nfi_token = explicit(cli.common.nfi_token_file.as_deref(), "NFI_TOKEN")?;
    let insecure = cli.common.allow_insecure;
    let creds =
        |u: &str, t: Option<String>, hint| Credentials::new("yoink", u, t, hint).allow_insecure(insecure);
    let need_url = || {
        url.clone()
            .context("no yoink service URL: set --url, YOINK_URL or `url` in the config file")
    };
    match cli.cmd {
        Some(Cmd::Login { force }) => {
            let url = need_url()?;
            let y = creds(&url, None, YOINK_HINT);
            eprintln!("logged in to {url} as {}", describe(&y.login(force).await?));
            if let Some(n) = &nfi_url {
                let c = creds(n, None, NFI_HINT);
                match c.config().await {
                    Ok(Some(cn)) => {
                        // A forced login already renewed a shared provider login.
                        let same = y
                            .config()
                            .await?
                            .is_some_and(|cy| (&cy.issuer, &cy.client_id) == (&cn.issuer, &cn.client_id));
                        eprintln!(
                            "logged in to {n} as {}",
                            describe(&c.login(force && !same).await?)
                        );
                    }
                    Ok(None) => {}
                    Err(e) => eprintln!("warning: not logging in to {n}: {e:#}"),
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Some(Cmd::Logout) => {
            let (mut done, mut ok) = (Vec::new(), true);
            for u in [url.clone(), nfi_url.clone()].into_iter().flatten() {
                // The first URL, or logged out with its provider login.
                if done.iter().any(|d: &String| d == u.trim_end_matches('/')) {
                    continue;
                }
                done.push(u.trim_end_matches('/').to_string());
                match nfi::logout(&creds(&u, None, ""), &u).await {
                    Ok(others) => done.extend(others),
                    Err(e) => {
                        ok = false;
                        eprintln!("error: logging out of {u}: {e:#}");
                    }
                }
            }
            Ok(if ok { ExitCode::SUCCESS } else { ExitCode::FAILURE })
        }
        Some(Cmd::Whoami) => {
            let mut targets = vec![(need_url()?, token, YOINK_HINT)];
            if let Some(n) = nfi_url {
                targets.push((n, nfi_token, NFI_HINT));
            }
            let mut ok = true;
            for (u, t, hint) in targets {
                match nfi::whoami(&creds(&u, t, hint), &u, false).await {
                    Ok(b) => ok &= b,
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
                allow_insecure: insecure,
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
