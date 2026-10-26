//! The yoink mirroring service, with the HTTP binary cache backend.

use std::process::ExitCode;
use std::sync::Arc;
use yoink_server::Mirror;

#[derive(clap::Subcommand, Debug)]
enum Backend {
    /// Copy from one HTTP binary cache to another.
    Http(yoink_mirror_http::Args),
}

fn main() -> ExitCode {
    yoink_server::main(|b: Backend| async move {
        Ok(match b {
            Backend::Http(a) => {
                Arc::new(yoink_mirror_http::HttpMirror::from_args(&a).await?) as Arc<dyn Mirror>
            }
        })
    })
}
