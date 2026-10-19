//! Helper probes: roots for the `helpers-direct` and `helpers-full` closure
//! types.
//!
//! nixpkgs helpers such as fetchurl build their tools inline (fetchurl uses
//! its own bootstrap curl), so those tools are in no runtime closure of
//! store-paths.xz. Probes (`probes.nix`) instantiate dummy helper calls
//! against a release's nixexprs; the build inputs they share become roots:
//!
//! - `helpers-direct`: outputs of the probes' direct input derivations, and
//!   of target probes (e.g. stdenv) themselves;
//! - `helpers-full`: outputs of every derivation in the probes' build graphs,
//!   including bootstrap stages and fixed-output sources.
//!
//! The roots go through the same closure walk as store-paths.xz roots.

use anyhow::{Context, Result, bail};
use futures::stream::{self, StreamExt};
use nfi_api::drv::ReducedGraph;
use nfi_interchange::StorePath;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Stdio;

pub const HELPERS_DIRECT: &str = "helpers-direct";
pub const HELPERS_FULL: &str = "helpers-full";

const PROBES_NIX: &str = include_str!("probes.nix");
/// Files the probes' dummy sources contain (for language vendoring helpers).
const SRC_FILES: &[&str] = &["Cargo.lock", "package-lock.json", "yarn.lock", "go.mod"];

#[derive(Clone, Debug)]
pub struct ProbeOptions {
    pub systems: Vec<String>,
    /// `nix-instantiate` command (and leading arguments).
    pub nix_instantiate: Vec<String>,
    /// `nix` command (and leading arguments), for `nix derivation show`.
    pub nix: Vec<String>,
    /// Probes instantiated concurrently.
    pub jobs: usize,
}

impl Default for ProbeOptions {
    fn default() -> Self {
        Self {
            systems: vec!["x86_64-linux".into(), "aarch64-linux".into()],
            nix_instantiate: vec!["nix-instantiate".into()],
            nix: vec!["nix".into()],
            jobs: 4,
        }
    }
}

/// The instantiated derivations of one probe.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProbeDrvs {
    /// The same helper called twice with different arguments.
    Fetch { a: String, b: String },
    /// Derivations that are roots themselves.
    Target(Vec<String>),
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProbeRoots {
    pub direct: BTreeSet<StorePath>,
    pub full: BTreeSet<StorePath>,
    /// `<probe>@<system>: <error>` for probes that could not be evaluated.
    pub skipped: Vec<String>,
}

/// Input derivations of `drv` and everything they depend on (not `drv`).
fn build_graph<'a>(graph: &'a ReducedGraph, drv: &'a str) -> BTreeSet<&'a str> {
    let mut seen = BTreeSet::new();
    let mut stack: Vec<&str> = graph
        .derivations
        .get(drv)
        .map(|d| d.input_drvs.keys().map(String::as_str).collect())
        .unwrap_or_default();
    while let Some(d) = stack.pop() {
        if seen.insert(d)
            && let Some(rd) = graph.derivations.get(d)
        {
            stack.extend(rd.input_drvs.keys().map(String::as_str));
        }
    }
    seen
}

/// Known output paths of `drv`, limited to `outputs` when given.
fn output_paths(graph: &ReducedGraph, drv: &str, outputs: Option<&BTreeSet<String>>) -> Vec<String> {
    graph
        .derivations
        .get(drv)
        .map(|d| {
            d.outputs
                .iter()
                .filter(|(name, _)| outputs.is_none_or(|o| o.contains(*name)))
                .filter_map(|(_, p)| p.clone())
                .collect()
        })
        .unwrap_or_default()
}

/// The direct and full root paths of one probe, as full store paths.
pub fn probe_roots(graph: &ReducedGraph, probe: &ProbeDrvs) -> (BTreeSet<String>, BTreeSet<String>) {
    let mut direct = BTreeSet::new();
    let mut full = BTreeSet::new();
    match probe {
        ProbeDrvs::Fetch { a, b } => {
            let (Some(da), Some(db)) = (graph.derivations.get(a), graph.derivations.get(b)) else {
                return (direct, full);
            };
            // Shared direct inputs, with the outputs either call consumes.
            for (input, outs_a) in &da.input_drvs {
                if let Some(outs_b) = db.input_drvs.get(input) {
                    let outs: BTreeSet<String> = outs_a.union(outs_b).cloned().collect();
                    direct.extend(output_paths(graph, input, Some(&outs)));
                }
            }
            let ga = build_graph(graph, a);
            let gb = build_graph(graph, b);
            for d in ga.intersection(&gb) {
                full.extend(output_paths(graph, d, None));
            }
        }
        ProbeDrvs::Target(targets) => {
            for t in targets {
                let outs = output_paths(graph, t, None);
                direct.extend(outs.iter().cloned());
                full.extend(outs);
                for d in build_graph(graph, t) {
                    full.extend(output_paths(graph, d, None));
                }
            }
        }
    }
    full.extend(direct.iter().cloned());
    (direct, full)
}

fn command(cmd: &[String]) -> Result<tokio::process::Command> {
    let Some((prog, pre)) = cmd.split_first() else {
        bail!("empty command")
    };
    let mut c = tokio::process::Command::new(prog);
    c.args(pre)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    Ok(c)
}

/// Run a command, returning stdout, or the tail of stderr as the error.
async fn run(mut cmd: tokio::process::Command, what: &str) -> Result<String> {
    let out = cmd.output().await.with_context(|| format!("running {what}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let msg: Vec<&str> = stderr.lines().filter(|l| !l.trim().is_empty()).collect();
        let tail = msg[msg.len().saturating_sub(3)..].join(" | ");
        bail!("{what} failed ({}): {tail}", out.status);
    }
    String::from_utf8(out.stdout).with_context(|| format!("{what}: non-UTF-8 output"))
}

struct ProbeFile {
    _dir: tempfile::TempDir,
    file: std::path::PathBuf,
    srcs: std::path::PathBuf,
}

fn write_probe_file() -> Result<ProbeFile> {
    let dir = tempfile::tempdir()?;
    let file = dir.path().join("probes.nix");
    std::fs::write(&file, PROBES_NIX)?;
    let srcs = dir.path().join("srcs");
    for x in ["a", "b"] {
        std::fs::create_dir_all(srcs.join(x))?;
        for f in SRC_FILES {
            std::fs::write(
                srcs.join(x).join(f),
                format!("# nix-fetch-indexer probe source {x}\n"),
            )?;
        }
    }
    Ok(ProbeFile {
        _dir: dir,
        file,
        srcs,
    })
}

fn instantiate_cmd(
    opts: &ProbeOptions,
    pf: &ProbeFile,
    nixpkgs: &Path,
    system: &str,
) -> Result<tokio::process::Command> {
    let mut c = command(&opts.nix_instantiate)?;
    c.arg(&pf.file)
        .arg("--arg")
        .arg("nixpkgs")
        .arg(nixpkgs)
        .arg("--argstr")
        .arg("system")
        .arg(system)
        .arg("--arg")
        .arg("srcs")
        .arg(&pf.srcs);
    Ok(c)
}

/// Instantiate one probe, returning its derivations.
async fn instantiate(
    opts: &ProbeOptions,
    pf: &ProbeFile,
    nixpkgs: &Path,
    system: &str,
    name: &str,
    kind: &str,
) -> Result<ProbeDrvs> {
    let mut c = instantiate_cmd(opts, pf, nixpkgs, system)?;
    let attrs: Vec<String> = match kind {
        "fetch" => vec![format!("probes'.{name}.a"), format!("probes'.{name}.b")],
        "target" => vec![format!("probes'.{name}.targets")],
        _ => bail!("unknown probe kind {kind:?}"),
    };
    for a in &attrs {
        c.arg("-A").arg(a);
    }
    let out = run(c, "nix-instantiate").await?;
    let drvs: Vec<String> = out
        .lines()
        .map(str::trim)
        .filter(|l| l.ends_with(".drv"))
        .map(|l| l.split('!').next().unwrap_or(l).to_string())
        .collect();
    match (kind, drvs.as_slice()) {
        ("fetch", [a, b]) => Ok(ProbeDrvs::Fetch {
            a: a.clone(),
            b: b.clone(),
        }),
        ("target", ts) if !ts.is_empty() => Ok(ProbeDrvs::Target(ts.to_vec())),
        _ => bail!("unexpected nix-instantiate output for {name}: {out:?}"),
    }
}

/// Run every probe for every system against an unpacked nixexprs tree.
pub async fn run_probes(nixpkgs: &Path, opts: &ProbeOptions) -> Result<ProbeRoots> {
    let pf = write_probe_file()?;
    let mut roots = ProbeRoots::default();
    for system in &opts.systems {
        let mut c = instantiate_cmd(opts, &pf, nixpkgs, system)?;
        c.args(["--eval", "--json", "--strict", "-A", "kinds"]);
        let kinds: BTreeMap<String, String> =
            serde_json::from_str(&run(c, "listing probes").await?).context("parsing probe list")?;
        let results: Vec<(String, Result<ProbeDrvs>)> = stream::iter(kinds.iter())
            .map(|(name, kind)| {
                let pf = &pf;
                async move {
                    let r = instantiate(opts, pf, nixpkgs, system, name, kind).await;
                    (name.clone(), r)
                }
            })
            .buffer_unordered(opts.jobs.max(1))
            .collect()
            .await;
        let mut probes = Vec::new();
        for (name, r) in results {
            match r {
                Ok(p) => probes.push((name, p)),
                Err(e) => {
                    tracing::warn!(probe = %name, %system, error = %format!("{e:#}"), "probe skipped");
                    roots.skipped.push(format!("{name}@{system}: {e:#}"));
                }
            }
        }
        if probes.is_empty() {
            continue;
        }
        let drvs: Vec<&String> = probes
            .iter()
            .flat_map(|(_, p)| match p {
                ProbeDrvs::Fetch { a, b } => vec![a, b],
                ProbeDrvs::Target(ts) => ts.iter().collect(),
            })
            .collect();
        let mut c = command(&opts.nix)?;
        c.args([
            "--extra-experimental-features",
            "nix-command",
            "derivation",
            "show",
            "--recursive",
        ])
        .args(&drvs);
        let json: serde_json::Value = serde_json::from_str(&run(c, "nix derivation show").await?)
            .context("parsing nix derivation show output")?;
        let graph = ReducedGraph::from_json(&json)?;
        let (mut nd, mut nf) = (0, 0);
        for (name, p) in &probes {
            let (direct, full) = probe_roots(&graph, p);
            tracing::debug!(probe = %name, %system, direct = direct.len(), full = full.len(), "probe roots");
            for (set, paths) in [(&mut roots.direct, direct), (&mut roots.full, full)] {
                for path in paths {
                    set.insert(
                        StorePath::parse(&path).with_context(|| format!("probe {name} output {path}"))?,
                    );
                }
            }
            nd = roots.direct.len();
            nf = roots.full.len();
        }
        tracing::info!(%system, probes = probes.len(), direct = nd, full = nf, "helper probes evaluated");
    }
    roots.skipped.sort();
    Ok(roots)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nfi_api::drv::ReducedDrv;

    fn p(n: &str) -> String {
        format!(
            "/nix/store/{}-{n}",
            n.chars().next().unwrap().to_string().repeat(32)
        )
    }

    fn drv(outs: &[(&str, &str)], inputs: &[(&str, &[&str])]) -> ReducedDrv {
        ReducedDrv {
            outputs: outs
                .iter()
                .map(|(o, path)| (o.to_string(), Some(p(path))))
                .collect(),
            input_drvs: inputs
                .iter()
                .map(|(d, o)| (p(d), o.iter().map(|x| x.to_string()).collect()))
                .collect(),
            input_srcs: Default::default(),
            fixed: false,
        }
    }

    /// Two fetches (a, b) sharing curl (built from a bootstrap stage) and a
    /// source tarball for curl; each also has its own argument-specific
    /// helper derivation.
    fn graph() -> ReducedGraph {
        let mut g = ReducedGraph::default();
        g.derivations.insert(
            p("a.drv"),
            drv(
                &[("out", "a-src")],
                &[("curl.drv", &["bin"]), ("x-a.drv", &["out"])],
            ),
        );
        g.derivations.insert(
            p("b.drv"),
            drv(
                &[("out", "b-src")],
                &[("curl.drv", &["bin", "out"]), ("y-b.drv", &["out"])],
            ),
        );
        g.derivations.insert(
            p("curl.drv"),
            drv(
                &[("bin", "curl-bin"), ("out", "curl-out"), ("dev", "curl-dev")],
                &[("stage0.drv", &["out"]), ("tarball.drv", &["out"])],
            ),
        );
        g.derivations
            .insert(p("stage0.drv"), drv(&[("out", "stage0")], &[]));
        g.derivations
            .insert(p("tarball.drv"), drv(&[("out", "tarball")], &[]));
        g.derivations
            .insert(p("x-a.drv"), drv(&[("out", "x-a")], &[("stage0.drv", &["out"])]));
        g.derivations.insert(p("y-b.drv"), drv(&[("out", "y-b")], &[]));
        g
    }

    #[test]
    fn fetch_probe_keeps_shared_inputs_only() {
        let g = graph();
        let (direct, full) = probe_roots(
            &g,
            &ProbeDrvs::Fetch {
                a: p("a.drv"),
                b: p("b.drv"),
            },
        );
        // Consumed outputs of the shared direct input only.
        assert_eq!(direct, BTreeSet::from([p("curl-bin"), p("curl-out")]));
        // Everything both build graphs share, all outputs, but not the
        // argument-specific derivations or the fetches themselves.
        assert_eq!(
            full,
            BTreeSet::from([
                p("curl-bin"),
                p("curl-out"),
                p("curl-dev"),
                p("stage0"),
                p("tarball")
            ])
        );
    }

    #[test]
    fn target_probe_is_its_own_root() {
        let g = graph();
        let (direct, full) = probe_roots(&g, &ProbeDrvs::Target(vec![p("curl.drv")]));
        assert_eq!(
            direct,
            BTreeSet::from([p("curl-bin"), p("curl-out"), p("curl-dev")])
        );
        assert_eq!(
            full,
            BTreeSet::from([
                p("curl-bin"),
                p("curl-out"),
                p("curl-dev"),
                p("stage0"),
                p("tarball")
            ])
        );
    }
}
