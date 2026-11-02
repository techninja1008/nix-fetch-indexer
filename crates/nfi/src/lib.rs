//! Support code for the `nfi` query CLI, and its API client.

pub mod client;
pub mod http;

use anyhow::{Context, Result, bail};
use nfi_api::drv::ReducedGraph;
use nfi_api::{BulkItem, EvalInfo, ReleaseInfo};
use serde::Deserialize;
use serde::de::DeserializeOwned;
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

/// `$XDG_CONFIG_HOME/<app>/config.toml` (or under `~/.config`).
pub fn config_path(app: &str) -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join(app).join("config.toml"))
}

pub fn default_config_path() -> Option<PathBuf> {
    config_path("nfi")
}

/// Load the TOML config `path`, or `app`'s default file if it exists.
pub fn load_toml<T: DeserializeOwned + Default>(app: &str, path: Option<&Path>) -> Result<T> {
    let (path, explicit) = match path {
        Some(p) => (p.to_path_buf(), true),
        None => match config_path(app) {
            Some(p) => (p, false),
            None => return Ok(T::default()),
        },
    };
    match std::fs::read_to_string(&path) {
        Ok(s) => toml::from_str(&s).with_context(|| format!("parsing {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && !explicit => Ok(T::default()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

pub fn load_config(path: Option<&Path>) -> Result<Config> {
    load_toml("nfi", path)
}

pub fn print_json<T: serde::Serialize>(v: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(v)?);
    Ok(())
}

/// `whoami` for the service at `url`, without logging in: prints who it
/// sees (`json`: the whole response). False if not authenticated.
pub async fn whoami(creds: &nfi_auth_client::Credentials, url: &str, json: bool) -> Result<bool> {
    let r = client::Client::new(url, creds.current().await?)?
        .get_json::<nfi_auth::Whoami>(nfi_auth::WHOAMI_PATH)
        .await;
    let w = match r {
        Ok(w) => w,
        Err(e) => match e.downcast_ref::<http::HttpError>().map(|h| h.status) {
            Some(401) => {
                println!("{url}: not authenticated");
                return Ok(false);
            }
            Some(404) => {
                println!("{url}: does not use login");
                return Ok(true);
            }
            _ => return Err(e),
        },
    };
    if json {
        return print_json(&w).map(|()| true);
    }
    println!("{url}: {}", nfi_auth_client::describe(&w));
    if !w.additional_info.is_empty() {
        for l in serde_json::to_string_pretty(&w.additional_info)?.lines() {
            println!("  {l}");
        }
    }
    Ok(true)
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
    nfi_drv::reduce(&v)
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
    nfi_drv::reduce(&v)
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

/// The error count of `e`, falling back to the listed errors (services
/// that predate `error_count`).
pub fn error_count(e: &EvalInfo) -> Option<u64> {
    e.error_count
        .or_else(|| e.errors.as_ref().map(|e| e.len() as u64))
}

/// A human-readable table of releases, one per line after a header; listed
/// errors follow their release, one evaluation per line.
pub fn releases_table(rels: &[ReleaseInfo]) -> String {
    let mut out = format!(
        "{:<40} {:<18} {:<12} {:<42} {:<28} EVALS\n",
        "RELEASE", "CHANNEL", "SERIES", "COMMIT", "DATASETS"
    );
    for r in rels {
        // e.g. `helpers[direct,full] (2 errors)`, `store-paths`.
        let evals: Vec<String> = r
            .evals
            .iter()
            .map(|e| {
                let mut s = e.eval_type.clone();
                if e.closures.iter().any(|c| !c.is_empty()) {
                    s += &format!("[{}]", e.closures.join(","));
                }
                if let Some(n) = error_count(e) {
                    s += &format!(" ({n} errors)");
                }
                s
            })
            .collect();
        out += &format!(
            "{:<40} {:<18} {:<12} {:<42} {:<28} {}\n",
            r.name,
            r.channel,
            r.series,
            r.commit,
            r.datasets.join(","),
            evals.join(", ")
        );
        for e in &r.evals {
            if let Some(errors) = e.errors.as_ref().filter(|e| !e.is_empty()) {
                out += &format!("    {} errors: {}\n", e.eval_type, errors.join(" "));
            }
        }
    }
    out
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

    #[test]
    fn releases_table_counts_and_lists_errors() {
        let eval = |errors: Option<Vec<&str>>| EvalInfo {
            eval_type: "helpers".into(),
            closures: vec!["direct".into()],
            error_count: Some(2),
            errors: errors.map(|e| e.into_iter().map(String::from).collect()),
        };
        let mut r = ReleaseInfo {
            name: "nixos-25.05.1.aaaa".into(),
            commit: "a".repeat(40),
            channel: "nixos-25.05".into(),
            series: "25.05".into(),
            url: None,
            datasets: vec!["closure".into()],
            evals: vec![eval(None)],
            indexed_at: chrono::DateTime::UNIX_EPOCH,
        };
        let t = releases_table(std::slice::from_ref(&r));
        assert_eq!(t.lines().count(), 2, "{t}");
        assert!(t.contains("helpers[direct] (2 errors)"), "{t}");
        r.evals = vec![eval(Some(vec!["a@x86_64-linux", "b@aarch64-linux"]))];
        let t = releases_table(&[r]);
        assert!(
            t.ends_with("    helpers errors: a@x86_64-linux b@aarch64-linux\n"),
            "{t}"
        );
    }
}
