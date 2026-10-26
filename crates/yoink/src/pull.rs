//! `yoink <TARGET>...`: resolve targets to PURLs, start the mirror request
//! and monitor it until every PURL has a result.

use crate::auth::with_login;
use crate::plain::{Plain, summary};
use crate::state::{Conn, PlanSummary, State};
use crate::stream::{Msg, Refresh, drive};
use crate::targets::{Target, classify, closure_purls, dedupe};
use crate::tui;
use anyhow::{Context, Result, bail};
use nfi_auth::client::{Session, interactive};
use std::io::IsTerminal;
use std::process::ExitCode;
use std::time::Duration;
use yoink_api::Outcome;

const YOINK_HINT: &str = "run `yoink login` or set YOINK_TOKEN";
const NFI_HINT: &str = "run `yoink login` (with the NFI URL configured) or set NFI_TOKEN";

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
    session: &Session,
    o: &mut PullOptions,
    flakes: Vec<String>,
) -> Result<(Vec<String>, PlanSummary)> {
    let Some(nfi_url) = o.nfi_url.clone() else {
        bail!(
            "flake references need the NFI service: set --nfi-url, NFI_URL or `nfi_url` in the config file"
        );
    };
    let nix = o.nix.clone();
    let graph = tokio::task::spawn_blocking(move || nfi::derivation_graph(&nix, &flakes)).await??;
    let mut token = session.bearer(&nfi_url, o.nfi_token.clone()).await?;
    let cmt = o.close_miss_threshold;
    let r = with_login(session, &nfi_url, &mut token, NFI_HINT, |t| {
        let (graph, url) = (&graph, &nfi_url);
        async move { nfi_api::client::Client::new(url, t)?.minimise(graph, cmt).await }
    })
    .await
    .context("minimising against NFI")?;
    o.nfi_token = token;
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

pub async fn pull(mut o: PullOptions) -> Result<ExitCode> {
    let targets = o
        .targets
        .iter()
        .map(|t| classify(t))
        .collect::<Result<Vec<_>>>()?;
    let session = Session::new(nfi_auth::client::TokenStore::default_location()?)?;
    let flakes: Vec<String> = targets
        .iter()
        .filter_map(|t| match t {
            Target::Flake(f) => Some(f.clone()),
            Target::Purl(_) => None,
        })
        .collect();
    let (closure, plan) = if flakes.is_empty() {
        (vec![], None)
    } else {
        let (c, p) = resolve_flakes(&session, &mut o, flakes).await?;
        (c, Some(p))
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

    // The service always requires a token.
    let explicit = o.token.is_some();
    let mut token = session.bearer(&o.url, o.token.clone()).await?;
    if token.is_none() && interactive() {
        token = session.relogin(&o.url, false).await?;
    }
    if token.is_none() {
        bail!("{} requires authentication; {YOINK_HINT}", o.url);
    }
    let mut client = yoink_api::client::Client::new(&o.url, token.clone())?;
    let mut first = client.mirror(&purls).await;
    if let Err(e) = &first
        && crate::auth::is_unauthorized(e)
        && let Some(t) = session.relogin(&o.url, true).await?
    {
        client.set_token(Some(t));
        first = client.mirror(&purls).await;
    }

    let mut state = State::new(&purls);
    state.plan = plan;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let refresh = (!explicit).then(|| Refresh {
        session: session.clone(),
        url: o.url.clone(),
    });
    let driver = tokio::spawn(drive(client, purls, Some(first), refresh, tx));

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
    if quit {
        eprintln!("{}", summary(&state));
        eprintln!("detached; the service keeps mirroring the remaining paths");
        return Ok(ExitCode::from(130));
    }
    if use_tui {
        eprintln!("{}", summary(&state));
        if let Conn::Failed(e) = &state.conn {
            eprintln!("error: {e}");
        }
    } else {
        eprintln!("{}", summary(&state));
    }
    let failed = state.items.iter().any(|i| i.outcome != Some(Outcome::Success));
    Ok(if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}
