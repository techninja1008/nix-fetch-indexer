//! Out-path indexer: evaluates the release's nixexprs with nix-eval-jobs.

use anyhow::{Context, Result, bail};
use nfi_interchange::{Bitset, Outpaths};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};

#[derive(Clone, Debug)]
pub struct OutpathsOptions {
    /// Command (and leading arguments) to run nix-eval-jobs.
    pub command: Vec<String>,
    pub systems: Vec<String>,
    pub workers: usize,
    pub max_memory_mb: usize,
}

impl Default for OutpathsOptions {
    fn default() -> Self {
        Self {
            command: vec!["nix-eval-jobs".into()],
            systems: vec!["x86_64-linux".into()],
            workers: 4,
            max_memory_mb: 4096,
        }
    }
}

#[derive(Deserialize)]
struct EvalLine {
    #[serde(default)]
    attr: Option<String>,
    #[serde(default, rename = "attrPath")]
    attr_path: Option<Vec<String>>,
    #[serde(default, rename = "drvPath")]
    drv_path: Option<String>,
    #[serde(default)]
    outputs: Option<BTreeMap<String, Option<String>>>,
    #[serde(default)]
    system: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

/// The Nix expression evaluated for a system: nixpkgs with the same config
/// used to generate packages.json.
pub fn eval_expr(nixexprs: &Path, system: &str) -> String {
    let d = nixexprs.display();
    format!(
        r#"let
  cfgFile = {d}/pkgs/top-level/packages-config.nix;
  config = if builtins.pathExists cfgFile then import cfgFile else {{ allowAliases = false; }};
in import {d} {{ system = "{system}"; inherit config; }}"#
    )
}

/// Parse one line of nix-eval-jobs output. Returns `None` for error lines
/// and entries without outputs.
pub fn parse_line(line: &str, fallback_system: &str, release: usize) -> Result<Option<Outpaths>> {
    let l: EvalLine = serde_json::from_str(line).with_context(|| format!("nix-eval-jobs output {line:?}"))?;
    if let Some(e) = l.error {
        tracing::debug!(attr = ?l.attr, error = %e.lines().next().unwrap_or(""), "evaluation error");
        return Ok(None);
    }
    let attr = match (l.attr, l.attr_path) {
        (Some(a), _) if !a.is_empty() => a,
        (_, Some(p)) if !p.is_empty() => p.join("."),
        _ => return Ok(None),
    };
    let outputs: BTreeMap<String, String> = l
        .outputs
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(k, v)| v.map(|v| (k, v)))
        .collect();
    if outputs.is_empty() {
        return Ok(None);
    }
    Ok(Some(Outpaths {
        attr,
        system: l.system.unwrap_or_else(|| fallback_system.to_string()),
        drv_path: l.drv_path,
        outputs,
        releases: Bitset::single(release),
    }))
}

/// Run nix-eval-jobs for each configured system over an extracted nixexprs tree.
pub async fn evaluate(nixexprs: &Path, opts: &OutpathsOptions, release: usize) -> Result<Vec<Outpaths>> {
    let mut out = Vec::new();
    for system in &opts.systems {
        let Some((prog, pre)) = opts.command.split_first() else {
            bail!("empty nix-eval-jobs command")
        };
        let mut cmd = tokio::process::Command::new(prog);
        cmd.args(pre)
            .arg("--impure")
            .arg("--workers")
            .arg(opts.workers.to_string())
            .arg("--max-memory-size")
            .arg(opts.max_memory_mb.to_string())
            .arg("--expr")
            .arg(eval_expr(nixexprs, system))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        let mut child = cmd.spawn().with_context(|| format!("spawning {prog}"))?;
        let mut lines = BufReader::new(child.stdout.take().expect("piped")).lines();
        let (mut ok, mut errors) = (0usize, 0usize);
        while let Some(line) = lines.next_line().await? {
            if line.trim().is_empty() {
                continue;
            }
            match parse_line(&line, system, release)? {
                Some(e) => {
                    ok += 1;
                    out.push(e);
                }
                None => errors += 1,
            }
        }
        let status = child.wait().await?;
        if !status.success() {
            bail!("{prog} exited with {status} for {system}");
        }
        tracing::info!(%system, ok, skipped = errors, "evaluated out paths");
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    #[test]
    fn parses_lines() {
        let l = r#"{"attr":"hello","attrPath":["hello"],"drvPath":"/nix/store/aaa-hello.drv","name":"hello-2.12","outputs":{"out":"/nix/store/bbb-hello"},"system":"x86_64-linux"}"#;
        let e = super::parse_line(l, "x", 1).unwrap().unwrap();
        assert_eq!(e.attr, "hello");
        assert_eq!(e.outputs["out"], "/nix/store/bbb-hello");
        assert!(e.releases.get(1));
        assert!(
            super::parse_line(r#"{"attr":"bad","error":"boom"}"#, "x", 0)
                .unwrap()
                .is_none()
        );
        let p = super::parse_line(
            r#"{"attrPath":["a","b"],"outputs":{"out":"/nix/store/c-d"}}"#,
            "aarch64-linux",
            0,
        )
        .unwrap()
        .unwrap();
        assert_eq!(p.attr, "a.b");
        assert_eq!(p.system, "aarch64-linux");
    }
}
