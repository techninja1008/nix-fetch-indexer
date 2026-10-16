//! Support code for the `nfi` query CLI.

use anyhow::{Context, Result, bail};
use nfi_api::BulkItem;
use nfi_api::drv::ReducedGraph;
use serde::Deserialize;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Optional configuration file (`$XDG_CONFIG_HOME/nfi/config.toml`).
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub url: Option<String>,
    pub fetch_hook: Option<String>,
    pub system: Option<String>,
    pub nix: Option<String>,
}

pub fn default_config_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("nfi").join("config.toml"))
}

pub fn load_config(path: Option<&Path>) -> Result<Config> {
    let (path, explicit) = match path {
        Some(p) => (p.to_path_buf(), true),
        None => match default_config_path() {
            Some(p) => (p, false),
            None => return Ok(Config::default()),
        },
    };
    match std::fs::read_to_string(&path) {
        Ok(s) => toml::from_str(&s).with_context(|| format!("parsing {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && !explicit => Ok(Config::default()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Build the fetch hook command line: `{}` in any argument is replaced by
/// `file`; if no argument contains `{}`, the file is appended.
pub fn hook_argv(hook: &str, file: &Path) -> Result<Vec<String>> {
    let words = shell_words::split(hook).context("parsing fetch hook command")?;
    if words.is_empty() {
        bail!("empty fetch hook command");
    }
    let f = file.to_string_lossy();
    let mut substituted = false;
    let mut argv: Vec<String> = words
        .into_iter()
        .map(|w| {
            if w.contains("{}") {
                substituted = true;
                w.replace("{}", &f)
            } else {
                w
            }
        })
        .collect();
    if !substituted {
        argv.push(f.into_owned());
    }
    Ok(argv)
}

/// Write `paths` (one per line) to a temporary file and run the hook
/// synchronously with the CLI's stdin/stdout/stderr. Returns its exit code.
pub fn run_fetch_hook(hook: &str, paths: &[String]) -> Result<i32> {
    let mut f = tempfile::Builder::new()
        .prefix("nfi-paths-")
        .suffix(".txt")
        .tempfile()?;
    for p in paths {
        writeln!(f, "{p}")?;
    }
    f.flush()?;
    let argv = hook_argv(hook, f.path())?;
    let status = Command::new(&argv[0])
        .args(&argv[1..])
        .status()
        .with_context(|| format!("running fetch hook {:?}", argv[0]))?;
    Ok(status.code().unwrap_or(1))
}

/// Run `nix derivation show --recursive` on installables and reduce the result.
pub fn derivation_graph(nix: &str, installables: &[String]) -> Result<ReducedGraph> {
    let words = shell_words::split(nix).context("parsing nix command")?;
    let Some((prog, pre)) = words.split_first() else {
        bail!("empty nix command")
    };
    let out = Command::new(prog)
        .args(pre)
        .args([
            "--extra-experimental-features",
            "nix-command flakes",
            "derivation",
            "show",
            "--recursive",
        ])
        .args(installables)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit())
        .output()
        .with_context(|| format!("running {prog}"))?;
    if !out.status.success() {
        bail!("nix derivation show failed ({})", out.status);
    }
    let v: serde_json::Value =
        serde_json::from_slice(&out.stdout).context("parsing nix derivation show output")?;
    ReducedGraph::from_json(&v)
}

/// Read a derivation graph (raw `nix derivation show` JSON or reduced) from
/// a file or `-` for stdin.
pub fn read_graph(path: &str) -> Result<ReducedGraph> {
    let text = if path == "-" {
        std::io::read_to_string(std::io::stdin())?
    } else {
        std::fs::read_to_string(path).with_context(|| format!("reading {path}"))?
    };
    let v: serde_json::Value = serde_json::from_str(&text).with_context(|| format!("parsing {path}"))?;
    ReducedGraph::from_json(&v)
}

/// Parse a bulk item: `attr@commit[@system]` or whitespace-separated
/// `attr commit [system]`.
pub fn parse_item(s: &str, system: Option<&str>, outputs: &Option<Vec<String>>) -> Result<BulkItem> {
    let parts: Vec<&str> = if s.contains('@') {
        s.split('@').collect()
    } else {
        s.split_whitespace().collect()
    };
    match parts.as_slice() {
        [attr, commit] | [attr, commit, _] if !attr.is_empty() && !commit.is_empty() => Ok(BulkItem {
            attr: attr.to_string(),
            commit: commit.to_string(),
            system: parts.get(2).map(|s| s.to_string()).or(system.map(str::to_string)),
            outputs: outputs.clone(),
        }),
        _ => bail!("expected attr@commit[@system], got {s:?}"),
    }
}

/// Read bulk items from a file (`-` for stdin): JSON (`{"items": [...]}` or an
/// array) or one item per line (blank lines and `#` comments ignored).
pub fn read_items(path: &str, system: Option<&str>, outputs: &Option<Vec<String>>) -> Result<Vec<BulkItem>> {
    let text = if path == "-" {
        std::io::read_to_string(std::io::stdin())?
    } else {
        std::fs::read_to_string(path).with_context(|| format!("reading {path}"))?
    };
    let t = text.trim_start();
    if t.starts_with('{') {
        let r: nfi_api::BulkRequest = serde_json::from_str(t)?;
        return Ok(r.items);
    }
    if t.starts_with('[') {
        return Ok(serde_json::from_str(t)?);
    }
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| parse_item(l, system, outputs))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hook_substitution() {
        let p = Path::new("/tmp/x y.txt");
        assert_eq!(
            hook_argv("nix copy --from https://c", p).unwrap(),
            vec!["nix", "copy", "--from", "https://c", "/tmp/x y.txt"]
        );
        assert_eq!(
            hook_argv("sh -c 'cat {} | wc -l'", p).unwrap(),
            vec!["sh", "-c", "cat /tmp/x y.txt | wc -l"]
        );
        assert_eq!(
            hook_argv("fetch --list={}", p).unwrap(),
            vec!["fetch", "--list=/tmp/x y.txt"]
        );
        assert!(hook_argv("", p).is_err());
    }

    #[test]
    fn items() {
        let i = parse_item("hello@abc", Some("aarch64-linux"), &None).unwrap();
        assert_eq!(
            (i.attr.as_str(), i.commit.as_str(), i.system.as_deref()),
            ("hello", "abc", Some("aarch64-linux"))
        );
        let i = parse_item("python3Packages.requests abc x86_64-darwin", None, &None).unwrap();
        assert_eq!(i.system.as_deref(), Some("x86_64-darwin"));
        assert!(parse_item("nocommit", None, &None).is_err());
    }

    #[test]
    fn config() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("c.toml");
        std::fs::write(&p, "url = \"http://x\"\nfetch_hook = \"echo {}\"\n").unwrap();
        let c = load_config(Some(&p)).unwrap();
        assert_eq!(c.url.as_deref(), Some("http://x"));
        assert!(load_config(Some(&d.path().join("missing.toml"))).is_err());
        std::fs::write(&p, "bogus = 1\n").unwrap();
        assert!(load_config(Some(&p)).is_err());
    }
}
