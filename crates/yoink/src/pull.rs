//! `yoink <TARGET>...`: resolve targets to PURLs, start the mirror request
//! and monitor it until every PURL has a result.

use crate::plain::{Plain, summary};
use crate::state::{Conn, PlanSummary, State};
use crate::stream::{Msg, drive};
use crate::targets::{Target, classify, closure_purls, dedupe};
use crate::tui;
use anyhow::{Context, Result, bail};
use nfi_auth_client::Credentials;
use nfi_auth_client::http::retry;
use std::io::IsTerminal;
use std::process::ExitCode;
use std::time::Duration;
use yoink_api::Outcome;

pub const YOINK_HINT: &str = "set YOINK_TOKEN or pass --token-file";
pub const NFI_HINT: &str = "set NFI_TOKEN or pass --nfi-token-file";

pub struct PullOptions {
    pub targets: Vec<String>,
    pub url: String,
    pub nfi_url: Option<String>,
    pub nix: String,
    pub token: Option<String>,
    pub nfi_token: Option<String>,
    pub plain: bool,
    pub verbosity: i8,
    pub close_miss_threshold: Option<u64>,
}

/// Evaluate flake references and ask NFI which cached paths they need.
async fn resolve_flakes(
    creds: &Credentials,
    nfi_url: &str,
    o: &PullOptions,
    flakes: Vec<String>,
) -> Result<(Vec<String>, PlanSummary)> {
    let nix = o.nix.clone();
    let graph = tokio::task::spawn_blocking(move || nfi::derivation_graph(&nix, &flakes)).await??;
    let client = nfi::client::Client::new(nfi_url, None)?;
    let cmt = o.close_miss_threshold;
    let r = retry(creds, |t| async {
        client.with_token(t).minimise(&graph, cmt).await
    })
    .await
    .context("minimising against NFI")?;
    let mut warnings: Vec<String> = r.warnings.iter().map(|w| w.message.clone()).collect();
    warnings.extend(
        r.unresolved
            .iter()
            .map(|u| format!("input derivation not in graph: {u}")),
    );
    let plan = PlanSummary {
        to_build: r.to_build.len(),
        to_fetch: r.to_fetch.len(),
        closure: r.closure.len(),
        warnings,
    };
    Ok((closure_purls(&r.closure), plan))
}

pub async fn pull(o: PullOptions) -> Result<ExitCode> {
    let targets = o
        .targets
        .iter()
        .map(|t| classify(t))
        .collect::<Result<Vec<_>>>()?;
    let flakes: Vec<String> = targets
        .iter()
        .filter_map(|t| match t {
            Target::Flake(f) => Some(f.clone()),
            Target::Purl(_) => None,
        })
        .collect();
    let nfi = match &o.nfi_url {
        _ if flakes.is_empty() => None,
        Some(u) => Some((
            u.as_str(),
            Credentials::new("yoink", u, o.nfi_token.clone(), NFI_HINT),
        )),
        None => bail!(
            "flake references need the NFI service: set --nfi-url, NFI_URL or `nfi_url` in the config file"
        ),
    };
    // Log in to both services up front, before evaluating anything and
    // before the monitor takes over the terminal.
    let creds = Credentials::new("yoink", &o.url, o.token.clone(), YOINK_HINT);
    creds.token(true).await?;
    if let Some((_, c)) = &nfi {
        c.token(true).await?;
    }
    let (closure, plan) = match &nfi {
        None => (vec![], None),
        Some((url, c)) => {
            let (c, p) = resolve_flakes(c, url, &o, flakes).await?;
            (c, Some(p))
        }
    };
    // Flake closures go where the first flake reference was given.
    let mut purls = Vec::new();
    let mut closure = Some(closure);
    for t in &targets {
        match t {
            Target::Purl(p) => purls.push(p.clone()),
            Target::Flake(_) => purls.extend(closure.take().into_iter().flatten()),
        }
    }
    let purls = dedupe(purls);

    let use_tui = !o.plain && std::io::stdout().is_terminal() && std::io::stderr().is_terminal();
    if let Some(p) = &plan
        && !use_tui
    {
        eprintln!(
            "nfi: {} cached paths, {} to build, {} to fetch",
            p.closure, p.to_build, p.to_fetch
        );
        for w in &p.warnings {
            eprintln!("warning: {w}");
        }
    }
    if purls.is_empty() {
        eprintln!("nothing to mirror");
        return Ok(ExitCode::SUCCESS);
    }

    let client = crate::client::Client::new(&o.url, None)?;
    let first = retry(&creds, |t| async { client.with_token(t).mirror(&purls).await }).await;

    let mut state = State::new(&purls);
    state.plan = plan;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let driver = tokio::spawn(drive(client, purls, Some(first), creds, tx));

    let (state, quit) = if use_tui {
        let app = tui::App::new(state, o.targets.join(" "), o.url.clone());
        tui::run(app, rx).await?
    } else {
        let mut plain = Plain::new(o.verbosity);
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        loop {
            tokio::select! {
                m = rx.recv() => match m {
                    None => break,
                    Some(Msg::Event(e)) => {
                        let c = state.apply(e);
                        if let Some(l) = plain.change(&state, &c) {
                            eprintln!("{l}");
                        }
                    }
                    Some(Msg::Conn(c)) => {
                        if let Some(l) = plain.conn(&c) {
                            eprintln!("{l}");
                        }
                        let end = matches!(c, Conn::Done | Conn::Failed(_));
                        state.conn = c;
                        if end {
                            break;
                        }
                    }
                },
                _ = tick.tick() => {
                    for l in plain.tick(&state, std::time::Instant::now()) {
                        eprintln!("{l}");
                    }
                }
            }
        }
        (state, false)
    };
    driver.abort();
    eprintln!("{}", summary(&state));
    if quit {
        eprintln!("detached; the service keeps mirroring the remaining paths");
        return Ok(ExitCode::from(130));
    }
    if use_tui && let Conn::Failed(e) = &state.conn {
        eprintln!("error: {e}");
    }
    let failed = state.items.iter().any(|i| i.outcome != Some(Outcome::Success));
    Ok(if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}
