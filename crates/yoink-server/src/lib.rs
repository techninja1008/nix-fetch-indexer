//! Core of the yoink mirroring service.
//!
//! The service is an HTTP API (see [`yoink_api`]) in front of a [`Mirror`]:
//! the backend that actually copies things. Backends live in their own
//! crates and are wired in by the binary, which hands [`main`] a clap
//! subcommand type (one variant per backend) and a function building the
//! chosen [`Mirror`]. Everything else (arguments, tracing, auth, the API) is
//! here.

pub mod api;
pub mod job;

pub use job::{Job, JobReporter, JobTable};

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use nfi_auth::server::{Authenticator, Expr, OidcSettings};
use std::future::Future;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use yoink_api::{PhaseDef, Purl};

/// A mirroring backend.
pub trait Mirror: Send + Sync + 'static {
    /// The phases this backend's jobs go through, sent first on every stream.
    fn phases(&self) -> Vec<PhaseDef>;

    /// Start mirroring `purl`, or attach to the job already doing so;
    /// deduplicating requests is the backend's job (see [`JobTable`]). An
    /// error (e.g. an unsupported PURL type) fails that PURL.
    fn mirror(&self, purl: &Purl) -> Result<Job>;
}

/// Authentication and the two authorisation policies.
///
/// Policy variables: `claims`, `user` and `request` (`{method, path}`) for
/// both; the per-PURL policy also has `purl`: `{type, namespace, name,
/// version, qualifiers, subpath, raw}`, where `raw` is the PURL as given.
pub struct Auth {
    pub authenticator: Authenticator,
    /// Evaluated once per API request.
    pub request: Expr,
    /// Evaluated for each PURL of a mirror request.
    pub purl: Expr,
}

/// Arguments shared by every yoink-service binary.
#[derive(Args, Debug)]
pub struct ServerArgs {
    /// Listen address.
    #[arg(long, env = "YOINK_LISTEN", default_value = "0.0.0.0:8090")]
    pub listen: String,
    /// OpenID provider issuer URL. ID tokens it issues to --oidc-client-id
    /// are accepted as bearer tokens.
    #[arg(long, env = "YOINK_OIDC_ISSUER")]
    pub oidc_issuer: String,
    /// Client id CLIs log in as (advertised to them).
    #[arg(long, env = "YOINK_OIDC_CLIENT_ID")]
    pub oidc_client_id: String,
    /// Expected token audience [default: the client id].
    #[arg(long, env = "YOINK_OIDC_AUDIENCE")]
    pub oidc_audience: Option<String>,
    /// Scopes CLIs request [default: openid,profile,offline_access].
    #[arg(long, env = "YOINK_OIDC_SCOPES", value_delimiter = ',')]
    pub oidc_scopes: Vec<String>,
    /// File with a CEL expression giving the username from `claims`
    /// [default: preferred_username, else sub].
    #[arg(long, env = "YOINK_OIDC_USERNAME_CEL_FILE")]
    pub oidc_username_cel_file: Option<PathBuf>,
    /// File with a CEL expression giving a credential or session id from
    /// `claims` (e.g. `claims.sid`), for logs.
    #[arg(long, env = "YOINK_OIDC_SESSION_CEL_FILE")]
    pub oidc_session_cel_file: Option<PathBuf>,
    /// File with a CEL expression (vars: claims, user, request) that must be
    /// true for every API request [default: true].
    #[arg(long, env = "YOINK_AUTHZ_CEL_FILE")]
    pub authz_cel_file: Option<PathBuf>,
    /// File with a CEL expression (vars: claims, user, request, purl) that
    /// must be true for each PURL to mirror [default: true].
    #[arg(long, env = "YOINK_PURL_AUTHZ_CEL_FILE")]
    pub purl_authz_cel_file: Option<PathBuf>,
    /// Most PURLs accepted in one mirror request.
    #[arg(long, env = "YOINK_MAX_PURLS", default_value_t = 100_000)]
    pub max_purls: usize,
}

#[derive(Parser, Debug)]
#[command(name = "yoink-service", version, about = "yoink mirroring service")]
struct Cli<B: Subcommand> {
    #[command(flatten)]
    server: ServerArgs,
    /// The mirror backend and its options.
    #[command(subcommand)]
    backend: B,
}

fn expr_or(path: &Option<PathBuf>, default: &str) -> Result<Expr> {
    match path {
        Some(p) => Expr::from_file(p),
        None => Expr::new(default),
    }
}

impl ServerArgs {
    pub fn auth(&self) -> Result<Auth> {
        let authenticator = Authenticator::new(OidcSettings {
            issuer: self.oidc_issuer.clone(),
            client_id: self.oidc_client_id.clone(),
            audience: self.oidc_audience.clone(),
            scopes: self.oidc_scopes.clone(),
            username: self
                .oidc_username_cel_file
                .as_deref()
                .map(Expr::from_file)
                .transpose()?,
            session: self
                .oidc_session_cel_file
                .as_deref()
                .map(Expr::from_file)
                .transpose()?,
        })?;
        Ok(Auth {
            authenticator,
            request: expr_or(&self.authz_cel_file, "true")?,
            purl: expr_or(&self.purl_authz_cel_file, "true")?,
        })
    }
}

/// Run a yoink-service binary: parse arguments (server options, then the
/// backend subcommand), set up tracing, build the mirror with `build` and
/// serve until SIGINT/SIGTERM.
pub fn main<B, F, Fut>(build: F) -> ExitCode
where
    B: Subcommand,
    F: FnOnce(B) -> Fut,
    Fut: Future<Output = Result<Arc<dyn Mirror>>>,
{
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let cli = Cli::<B>::parse();
    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("error: starting the runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    match rt.block_on(run(cli.server, build(cli.backend))) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!(error = %format!("{e:#}"), "fatal");
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(args: ServerArgs, mirror: impl Future<Output = Result<Arc<dyn Mirror>>>) -> Result<()> {
    let auth = args.auth()?;
    if let Err(e) = auth.authenticator.prefetch().await {
        tracing::warn!(error = %e, "could not fetch the OpenID provider's keys yet");
    }
    let mirror = mirror.await.context("starting the mirror backend")?;
    let state = api::State::new(mirror, auth, args.max_purls);
    let listener = tokio::net::TcpListener::bind(&args.listen)
        .await
        .with_context(|| format!("binding {}", args.listen))?;
    tracing::info!(addr = %listener.local_addr()?, "listening");
    let (tx, mut rx) = tokio::sync::watch::channel(());
    let server = async move {
        axum::serve(listener, api::router(state))
            .with_graceful_shutdown(async move {
                let _ = rx.changed().await;
            })
            .await
    };
    tokio::pin!(server);
    tokio::select! {
        r = &mut server => r?,
        _ = shutdown_signal() => {
            let _ = tx.send(());
            // Mirror streams can be long-lived; their jobs die with the
            // process anyway, and clients reconnect to another replica.
            let _ = tokio::time::timeout(std::time::Duration::from_secs(5), &mut server).await;
        }
    }
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    let mut term =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("SIGTERM handler");
    tokio::select! {
        _ = ctrl_c => {}
        _ = term.recv() => {}
    }
    tracing::info!("shutting down");
}
