//! After mirroring: make the mirrored store paths visible to Nix at once.
//!
//! Nix remembers binary cache misses for `narinfo-cache-negative-ttl`
//! (default 1 h), so a path looked up before it was mirrored would still
//! look absent. A trusted user can have Nix (or the daemon) look the paths
//! up again with that setting at 0 ([`Nix::refresh`]); an untrusted one
//! can't (the daemon ignores the restricted setting), and gets
//! [`UNTRUSTED_GUIDANCE`] instead. `--download` copies the paths into the
//! local store straight away ([`Nix::download`]), reading the cache
//! client-side, where the daemon's misses don't matter.

use anyhow::{Context, Result, bail};
use std::process::Stdio;
use tokio::io::AsyncWriteExt;

pub const UNTRUSTED_GUIDANCE: &str = "note: Nix may remember earlier misses of these paths for up to \
`narinfo-cache-negative-ttl` (default 1 h), and you are not a trusted Nix user, so yoink can't refresh them. \
To avoid that: add yourself to `trusted-users`, set `narinfo-cache-negative-ttl` low in the daemon's nix.conf, \
run `sudo rm /root/.cache/nix/binary-cache-v*.sqlite*`, or use `yoink --download`.";

/// The nix command, as given (`--nix`, config `nix`), split into words.
pub struct Nix {
    prog: String,
    pre: Vec<String>,
}

impl Nix {
    pub fn new(cmd: &str) -> Result<Self> {
        let words = shell_words::split(cmd).context("parsing the nix command")?;
        let Some((prog, pre)) = words.split_first() else {
            bail!("empty nix command")
        };
        Ok(Nix {
            prog: prog.clone(),
            pre: pre.to_vec(),
        })
    }

    fn command(&self, args: &[&str]) -> tokio::process::Command {
        let mut c = tokio::process::Command::new(&self.prog);
        c.args(&self.pre)
            .args(["--extra-experimental-features", "nix-command"])
            .args(args)
            .kill_on_drop(true);
        c
    }

    /// Whether the store (the daemon, for a multi-user install) trusts us.
    pub async fn trusted(&self) -> Result<bool> {
        let out = self
            .command(&["store", "info", "--json"])
            .stdin(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .await
            .with_context(|| format!("running {}", self.prog))?;
        if !out.status.success() {
            bail!(
                "nix store info failed ({}): {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        #[derive(serde::Deserialize)]
        struct Info {
            #[serde(default)]
            trusted: bool,
        }
        let i: Info = serde_json::from_slice(&out.stdout).context("parsing nix store info --json")?;
        Ok(i.trusted)
    }

    /// Look `paths` up again, overwriting cached misses; nothing is fetched.
    pub async fn refresh(&self, paths: &[String]) -> Result<()> {
        let mut c = self.command(&[
            "build",
            "--dry-run",
            "--option",
            "narinfo-cache-negative-ttl",
            "0",
            "--stdin",
        ]);
        c.stdout(Stdio::null()).stderr(Stdio::piped());
        let out = with_stdin(c, paths).await?;
        if !out.status.success() {
            bail!(
                "nix build --dry-run failed ({}): {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
    }

    /// Copy `paths` from the cache at `url` into the local store. Nix's own
    /// progress and errors go to stderr.
    pub async fn download(&self, url: &str, paths: &[String]) -> Result<()> {
        let mut c = self.command(&[
            "copy",
            "--from",
            url,
            "--option",
            "narinfo-cache-negative-ttl",
            "0",
            "--stdin",
        ]);
        c.stdout(Stdio::null()).stderr(Stdio::inherit());
        let out = with_stdin(c, paths).await?;
        if !out.status.success() {
            bail!(
                "nix copy --from {url} failed ({}); see its error above. The paths are mirrored, but not in \
                 the local store. Unless you are a trusted Nix user, Nix only imports paths signed by a key \
                 in `trusted-public-keys` (`require-sigs`)",
                out.status
            );
        }
        Ok(())
    }
}

/// Run `c` with `paths` on its stdin, one per line (no argv limits).
async fn with_stdin(mut c: tokio::process::Command, paths: &[String]) -> Result<std::process::Output> {
    let mut child = c.stdin(Stdio::piped()).spawn().context("running nix")?;
    let mut stdin = child.stdin.take().expect("piped");
    let input: String = paths.iter().map(|p| format!("{p}\n")).collect();
    // nix may exit without reading it all; its status says why.
    let _ = stdin.write_all(input.as_bytes()).await;
    drop(stdin);
    child.wait_with_output().await.context("waiting for nix")
}
