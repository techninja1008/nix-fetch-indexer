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
use clap::builder::Resettable;
use clap::{Args, Command, Parser, Subcommand};
use nfi_auth_server::{Authenticator, Expr, Guard};
use nfi_service_common::{OidcArgs, init_tracing, shutdown_signal};
use std::future::Future;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use yoink_api::PhaseDef;
use yoink_purl::Purl;

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
    /// OIDC (never anonymous) with the per-request policy.
    pub guard: Arc<Guard>,
    /// Evaluated for each PURL of a mirror request.
    pub purl: Expr,
}

impl Auth {
    /// `request` is evaluated once per API request, `purl` for each PURL of
    /// a mirror request.
    pub fn new(authenticator: Arc<Authenticator>, request: Expr, purl: Expr) -> Self {
        Auth {
            guard: Arc::new(Guard::Oidc {
                authenticator,
                policy: request,
                anonymous: false,
            }),
            purl,
        }
    }
}

/// Arguments shared by every yoink-service binary.
#[derive(Args, Debug)]
pub struct ServerArgs {
    /// Listen address.
    #[arg(long, env = "YOINK_LISTEN", default_value = "0.0.0.0:8090")]
    pub listen: String,
    #[command(flatten)]
    pub oidc: OidcArgs,
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

/// Name the OIDC flags' environment variables `YOINK_OIDC_*`, require the
/// issuer and client id, and describe the flags as they apply here.
fn server_command(cmd: Command) -> Command {
    OidcArgs::env(cmd, "YOINK_")
        .mut_arg("oidc_issuer", |a| {
            // Required, so its `requires` is moot (and would reorder the usage line).
            a.required(true).requires(Resettable::Reset).help(
                "OpenID provider issuer URL. ID tokens it issues to --oidc-client-id are accepted as bearer tokens",
            )
        })
        .mut_arg("oidc_client_id", |a| {
            a.required(true)
                .help("Client id CLIs log in as (advertised to them)")
        })
        .mut_arg("oidc_audience", |a| {
            a.help("Expected token audience [default: the client id]")
        })
        .mut_arg("oidc_username_cel_file", |a| {
            a.help("File with a CEL expression giving the username from `claims` [default: preferred_username, else sub]")
        })
        .mut_arg("oidc_session_cel_file", |a| {
            a.help("File with a CEL expression giving a credential or session id from `claims` (e.g. `claims.sid`), for logs")
        })
}

impl ServerArgs {
    /// Read the expressions and start the authenticator (fetching the
    /// provider's keys; see `Authenticator::start`).
    pub async fn auth(&self) -> Result<Auth> {
        let settings = self
            .oidc
            .settings()?
            .context("--oidc-issuer and --oidc-client-id are required")?;
        let request = Expr::from_file_or(self.authz_cel_file.as_deref(), "true")?;
        let purl = Expr::from_file_or(self.purl_authz_cel_file.as_deref(), "true")?;
        Ok(Auth::new(Authenticator::start(settings).await?, request, purl))
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
    init_tracing("info");
    let cli: Cli<B> = nfi_service_common::parse(server_command);
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
    let auth = args.auth().await?;
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
