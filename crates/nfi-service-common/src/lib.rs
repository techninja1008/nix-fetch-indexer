//! Shared by the nfi and yoink services: tracing set-up, waiting for a
//! shutdown signal, and the OIDC login flags.

use anyhow::Result;
use clap::{Args, Command, Parser};
use nfi_auth_server::OidcSettings;
use std::path::PathBuf;

/// Log to stderr, filtered by `RUST_LOG`, else by `default_filter` (e.g.
/// `info`).
pub fn init_tracing(default_filter: &str) {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| default_filter.into()),
        )
        .init();
}

/// Wait for SIGINT or SIGTERM.
pub async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    let mut term =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("SIGTERM handler");
    tokio::select! {
        _ = ctrl_c => {}
        _ = term.recv() => {}
    }
    tracing::info!("shutting down");
}

/// Parse the command line into `P`, after `adjust` changes its command
/// (e.g. with [`OidcArgs::env`]).
pub fn parse<P: Parser>(adjust: impl FnOnce(Command) -> Command) -> P {
    let mut cmd = adjust(P::command());
    let matches = cmd.clone().get_matches();
    P::from_arg_matches(&matches).unwrap_or_else(|e| e.format(&mut cmd).exit())
}

/// OIDC login flags. Their environment variables are named per service by
/// [`OidcArgs::env`]. The issuer and client id are optional and come
/// together; a service requiring them marks them required.
#[derive(Args, Debug)]
pub struct OidcArgs {
    /// OpenID provider issuer URL.
    #[arg(long, requires = "oidc_client_id")]
    pub oidc_issuer: Option<String>,
    /// Public client that CLIs log in as; ID tokens must be issued to it.
    #[arg(long, requires = "oidc_issuer")]
    pub oidc_client_id: Option<String>,
    /// Expected ID token audience [default: the client id].
    #[arg(long, requires = "oidc_issuer")]
    pub oidc_audience: Option<String>,
    /// Scopes CLIs request [default: openid,profile,offline_access].
    #[arg(long, value_delimiter = ',', requires = "oidc_issuer")]
    pub oidc_scopes: Vec<String>,
    /// File with a CEL expression over `claims` giving the username
    /// [default: preferred_username, else sub].
    #[arg(long, requires = "oidc_issuer")]
    pub oidc_username_cel_file: Option<PathBuf>,
    /// File with a CEL expression over `claims` giving a credential or
    /// session id to log (string or null).
    #[arg(long, requires = "oidc_issuer")]
    pub oidc_session_cel_file: Option<PathBuf>,
    /// File with a CEL expression over `claims` and `user` giving the map
    /// whoami reports as `additional_info` [default: claims].
    #[arg(long, requires = "oidc_issuer")]
    pub oidc_whoami_cel_file: Option<PathBuf>,
}

impl OidcArgs {
    /// Read the flags of `cmd`'s [`OidcArgs`] from `<prefix>OIDC_...`
    /// environment variables (e.g. `NFI_OIDC_ISSUER` for `--oidc-issuer`).
    pub fn env(cmd: Command, prefix: &str) -> Command {
        let ours = Self::augment_args(Command::new(""));
        ours.get_arguments().fold(cmd, |cmd, a| {
            let env = format!("{prefix}{}", a.get_id().as_str().to_uppercase());
            cmd.mut_arg(a.get_id(), |a| a.env(env))
        })
    }

    /// The settings, reading the CEL files, or `None` without an issuer.
    pub fn settings(&self) -> Result<Option<OidcSettings>> {
        let (Some(issuer), Some(client_id)) = (&self.oidc_issuer, &self.oidc_client_id) else {
            return Ok(None);
        };
        OidcSettings::from_files(
            issuer,
            client_id,
            self.oidc_audience.clone(),
            self.oidc_scopes.clone(),
            self.oidc_username_cel_file.as_deref(),
            self.oidc_session_cel_file.as_deref(),
            self.oidc_whoami_cel_file.as_deref(),
        )
        .map(Some)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[derive(Parser)]
    struct Cli {
        #[command(flatten)]
        oidc: OidcArgs,
        #[arg(long, env = "OTHER")]
        other: Option<String>,
    }

    #[test]
    fn env_names_follow_the_prefix() {
        let cmd = OidcArgs::env(Cli::command(), "SVC_");
        let env = |id: &str| {
            let a = cmd.get_arguments().find(|a| a.get_id() == id).unwrap();
            a.get_env().unwrap().to_str().unwrap().to_string()
        };
        assert_eq!(env("oidc_issuer"), "SVC_OIDC_ISSUER");
        assert_eq!(env("oidc_whoami_cel_file"), "SVC_OIDC_WHOAMI_CEL_FILE");
        assert_eq!(env("other"), "OTHER");
        cmd.debug_assert();
    }
}
