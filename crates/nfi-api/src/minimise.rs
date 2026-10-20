//! Minimise planning: which cached paths are useful for building a graph.

use crate::drv::ReducedGraph;
use std::collections::{BTreeMap, BTreeSet, HashSet};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    pub requested: Vec<String>,
    /// Derivations that must be built, in dependants-first order.
    pub to_build: Vec<String>,
    /// Fixed-output derivations (fetches) that must be realised, in
    /// dependants-first order. Their inputs (the fetch tools) are needed.
    pub to_fetch: Vec<String>,
    /// Cached paths that are directly needed (outputs of cache hits and
    /// cached input sources of derivations that must be built).
    pub hits: BTreeSet<String>,
    /// Needed input derivations missing from the graph.
    pub unresolved: Vec<String>,
}

/// Plan the build of every requested derivation.
///
/// Derivations are visited dependants-first. A needed derivation whose needed
/// outputs all have known, cached paths is a hit: its outputs are recorded
/// and its inputs are not visited. Otherwise it must be built, and its inputs
/// (input derivations with the outputs it consumes, and input sources)
/// become needed.
pub fn plan(graph: &ReducedGraph, is_cached: impl Fn(&str) -> bool) -> Plan {
    let requested = graph.requested();
    let order = topo_order(graph, &requested);
    let mut needed: BTreeMap<&str, BTreeSet<String>> = BTreeMap::new();
    for r in &requested {
        if let Some(d) = graph.derivations.get(r) {
            needed.insert(r.as_str(), d.outputs.keys().cloned().collect());
        }
    }
    let mut plan = Plan {
        requested: requested.clone(),
        ..Default::default()
    };
    let mut unresolved = BTreeSet::new();
    for drv in order {
        let Some(outs) = needed.remove(drv) else { continue };
        let Some(d) = graph.derivations.get(drv) else {
            unresolved.insert(drv.to_string());
            continue;
        };
        let paths: Option<Vec<&String>> = outs
            .iter()
            .map(|o| d.outputs.get(o).and_then(|p| p.as_ref()))
            .collect();
        if let Some(paths) = paths.filter(|ps| !ps.is_empty() && ps.iter().all(|p| is_cached(p))) {
            plan.hits.extend(paths.into_iter().cloned());
            continue;
        }
        if d.fixed {
            plan.to_fetch.push(drv.to_string());
        } else {
            plan.to_build.push(drv.to_string());
        }
        for (input, input_outs) in &d.input_drvs {
            let e = needed.entry(input.as_str()).or_default();
            e.extend(input_outs.iter().cloned());
            if !graph.derivations.contains_key(input) {
                unresolved.insert(input.clone());
            }
        }
        for src in &d.input_srcs {
            if is_cached(src) {
                plan.hits.insert(src.clone());
            }
        }
    }
    plan.unresolved = unresolved.into_iter().collect();
    plan
}

pub const UNINDEXED_NIXPKGS: &str = "unindexed_nixpkgs";

/// Whether a derivation path is a final standard environment
/// (`<hash>-stdenv-linux.drv` / `<hash>-stdenv-darwin.drv`, not a bootstrap stage).
fn is_stdenv_drv(drv: &str) -> bool {
    let base = drv.rsplit('/').next().unwrap_or(drv);
    let name = base.get(33..).unwrap_or("");
    matches!(name, "stdenv-linux.drv" | "stdenv-darwin.drv")
}

/// Anchor check: Hydra builds stdenv for every release, so a stdenv that
/// must be built almost always means the graph was evaluated from a nixpkgs
/// revision that is not indexed (or with overlays/config that change
/// stdenv). Only stdenvs the plan needs are considered: one beneath a cached
/// path does not matter.
pub fn anchor_warnings(plan: &Plan) -> Vec<crate::Warning> {
    // Every needed final stdenv is reported: graphs can legitimately mix
    // several nixpkgs instances, and each unindexed one deserves a warning.
    let uncached: Vec<String> = plan
        .to_build
        .iter()
        .filter(|drv| is_stdenv_drv(drv))
        .cloned()
        .collect();
    if uncached.is_empty() {
        return Vec::new();
    }
    vec![crate::Warning {
        code: UNINDEXED_NIXPKGS.into(),
        message: format!(
            "the standard environment ({}) is not in the index: this graph was most likely evaluated from a \
             nixpkgs revision that is not indexed (or with overlays/config that change stdenv), so far more \
             will need to be built than with an indexed release",
            uncached
                .iter()
                .map(|d| d.rsplit('/').next().unwrap_or(d))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        paths: uncached,
    }]
}

pub const CLOSE_MISS: &str = "close_miss";
/// Default `close_miss_threshold`: names shared by this many indexed paths
/// or more are too generic (e.g. `source`) to suggest anything.
pub const DEFAULT_CLOSE_MISS_THRESHOLD: u64 = 100;

/// The name of a derivation (`<hash>-<name>.drv`), which is also the name of
/// its `out` output's path.
pub fn drv_name(drv: &str) -> &str {
    let base = drv.rsplit('/').next().unwrap_or(drv);
    let base = base.strip_suffix(".drv").unwrap_or(base);
    base.get(33..).unwrap_or(base)
}

/// Derivations to build whose name matches indexed paths: the same package
/// (name and version) is indexed but with a different hash, e.g. a slightly
/// overridden package or a build tool from a nixpkgs helper that is not
/// indexed. `indexed` gives the number of indexed paths with a name; names
/// matching `threshold` or more paths (and 0, which disables the check) are
/// ignored.
pub fn close_miss_warnings(
    plan: &Plan,
    indexed: impl Fn(&str) -> u64,
    threshold: u64,
) -> Vec<crate::Warning> {
    if threshold == 0 {
        return Vec::new();
    }
    let misses: Vec<(&String, u64)> = plan
        .to_build
        .iter()
        .map(|d| (d, indexed(drv_name(d))))
        .filter(|&(_, n)| n > 0 && n < threshold)
        .collect();
    if misses.is_empty() {
        return Vec::new();
    }
    let examples: Vec<String> = misses
        .iter()
        .take(5)
        .map(|(d, n)| format!("{} ({n} indexed)", drv_name(d)))
        .collect();
    vec![crate::Warning {
        code: CLOSE_MISS.into(),
        message: format!(
            "{} derivation(s) to build look like indexed paths with a different hash, e.g. {}: they are probably \
             overridden packages, or build inputs that are not indexed",
            misses.len(),
            examples.join(", ")
        ),
        paths: misses.into_iter().map(|(d, _)| d.clone()).collect(),
    }]
}

/// Dependants-first order of every derivation reachable from `roots`
/// (reverse DFS post-order). Missing derivations are included as leaves.
fn topo_order<'a>(graph: &'a ReducedGraph, roots: &'a [String]) -> Vec<&'a str> {
    let mut post = Vec::new();
    let mut visited: HashSet<&str> = HashSet::new();
    for root in roots {
        if !visited.insert(root.as_str()) {
            continue;
        }
        // Stack of (node, iterator position over its inputs).
        let mut stack: Vec<(&str, Vec<&str>)> = vec![(root.as_str(), inputs(graph, root))];
        while let Some((node, children)) = stack.last_mut() {
            if let Some(c) = children.pop() {
                if visited.insert(c) {
                    let ch = inputs(graph, c);
                    stack.push((c, ch));
                }
            } else {
                post.push(*node);
                stack.pop();
            }
        }
    }
    post.reverse();
    post
}

fn inputs<'a>(graph: &'a ReducedGraph, drv: &str) -> Vec<&'a str> {
    graph
        .derivations
        .get(drv)
        .map(|d| d.input_drvs.keys().map(String::as_str).collect())
        .unwrap_or_default()
}

/// Given the closure edges of the hit set, the hits that are not contained in
/// another hit's runtime closure.
pub fn roots(hits: &BTreeSet<String>, edges: &[(String, String)]) -> Vec<String> {
    let mut adj: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (a, b) in edges {
        if a != b {
            adj.entry(a.as_str()).or_default().push(b.as_str());
        }
    }
    let mut reached: HashSet<&str> = HashSet::new();
    let mut stack: Vec<&str> = hits
        .iter()
        .flat_map(|h| adj.get(h.as_str()).cloned().unwrap_or_default())
        .collect();
    while let Some(n) = stack.pop() {
        if reached.insert(n)
            && let Some(next) = adj.get(n)
        {
            stack.extend(next.iter().copied());
        }
    }
    hits.iter()
        .filter(|h| !reached.contains(h.as_str()))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drv::{ReducedDrv, ReducedGraph};

    fn drv(outs: &[(&str, Option<&str>)], inputs: &[(&str, &[&str])], srcs: &[&str]) -> ReducedDrv {
        ReducedDrv {
            outputs: outs
                .iter()
                .map(|(n, p)| (n.to_string(), p.map(str::to_string)))
                .collect(),
            input_drvs: inputs
                .iter()
                .map(|(d, o)| (d.to_string(), o.iter().map(|s| s.to_string()).collect()))
                .collect(),
            input_srcs: srcs.iter().map(|s| s.to_string()).collect(),
            fixed: false,
        }
    }

    /// app (uncached) -> lib (uncached) -> glibc (cached)
    ///                -> tool (cached) -> deep (cached, must not be listed)
    ///                -> src.tar (cached input source)
    /// lib also consumes only `dev` from openssl (cached dev, uncached out).
    fn graph() -> ReducedGraph {
        let mut g = ReducedGraph::default();
        let d = &mut g.derivations;
        d.insert(
            "app.drv".into(),
            drv(
                &[("out", Some("app"))],
                &[("lib.drv", &["out"]), ("tool.drv", &["out"])],
                &["src.tar"],
            ),
        );
        d.insert(
            "lib.drv".into(),
            drv(
                &[("out", Some("lib"))],
                &[("glibc.drv", &["out"]), ("openssl.drv", &["dev"])],
                &[],
            ),
        );
        d.insert(
            "glibc.drv".into(),
            drv(&[("out", Some("glibc"))], &[("bootstrap.drv", &["out"])], &[]),
        );
        d.insert(
            "openssl.drv".into(),
            drv(
                &[("out", Some("openssl")), ("dev", Some("openssl-dev"))],
                &[("deep.drv", &["out"])],
                &[],
            ),
        );
        d.insert(
            "tool.drv".into(),
            drv(&[("out", Some("tool"))], &[("deep.drv", &["out"])], &[]),
        );
        d.insert("deep.drv".into(), drv(&[("out", Some("deep"))], &[], &[]));
        d.insert(
            "bootstrap.drv".into(),
            drv(&[("out", Some("bootstrap"))], &[], &[]),
        );
        g
    }

    #[test]
    fn plans_minimal_hits() {
        let cached = ["glibc", "openssl-dev", "tool", "deep", "src.tar", "bootstrap"];
        let p = plan(&graph(), |p| cached.contains(&p));
        assert_eq!(p.requested, vec!["app.drv"]);
        assert_eq!(p.to_build, vec!["app.drv", "lib.drv"]);
        let hits: Vec<_> = p.hits.iter().map(String::as_str).collect();
        assert_eq!(hits, vec!["glibc", "openssl-dev", "src.tar", "tool"]);
        assert!(p.unresolved.is_empty());
    }

    #[test]
    fn needed_outputs_accumulate_before_visit() {
        // Two builders consume different outputs of openssl; both must be cached for a hit.
        let mut g = graph();
        g.derivations.insert(
            "top.drv".into(),
            drv(
                &[("out", Some("top"))],
                &[("app.drv", &["out"]), ("other.drv", &["out"])],
                &[],
            ),
        );
        g.derivations.insert(
            "other.drv".into(),
            drv(&[("out", Some("other"))], &[("openssl.drv", &["out"])], &[]),
        );
        let cached = ["glibc", "openssl-dev", "tool", "deep"];
        let p = plan(&g, |p| cached.contains(&p));
        // openssl.out is not cached, so openssl must be built, and deep becomes needed.
        assert!(p.to_build.contains(&"openssl.drv".to_string()));
        assert!(p.hits.contains("deep"));
        assert!(!p.hits.contains("openssl-dev"));
    }

    #[test]
    fn ca_outputs_and_unresolved() {
        let mut g = ReducedGraph::default();
        g.derivations.insert(
            "a.drv".into(),
            drv(&[("out", None)], &[("gone.drv", &["out"])], &[]),
        );
        let p = plan(&g, |_| true);
        assert_eq!(p.to_build, vec!["a.drv"]);
        assert_eq!(p.unresolved, vec!["gone.drv"]);
    }

    #[test]
    fn everything_cached() {
        let p = plan(&graph(), |_| true);
        assert!(p.to_build.is_empty());
        assert_eq!(p.hits.into_iter().collect::<Vec<_>>(), vec!["app"]);
    }

    #[test]
    fn fixed_outputs_are_fetches() {
        let mut g = graph();
        // lib fetches its source; the fetch needs curl.
        g.derivations
            .get_mut("lib.drv")
            .unwrap()
            .input_drvs
            .insert("src.drv".into(), ["out".to_string()].into());
        let mut src = drv(&[("out", Some("src"))], &[("curl.drv", &["bin"])], &[]);
        src.fixed = true;
        g.derivations.insert("src.drv".into(), src);
        g.derivations
            .insert("curl.drv".into(), drv(&[("bin", Some("curl-bin"))], &[], &[]));
        let cached = ["glibc", "openssl-dev", "tool", "deep", "src.tar", "curl-bin"];
        let p = plan(&g, |p| cached.contains(&p));
        assert_eq!(p.to_build, vec!["app.drv", "lib.drv"]);
        assert_eq!(p.to_fetch, vec!["src.drv"]);
        assert!(p.hits.contains("curl-bin"));
        // A cached fetch is a hit like anything else.
        let p = plan(&g, |p| cached.contains(&p) || p == "src");
        assert!(p.to_fetch.is_empty());
        assert!(p.hits.contains("src"));
    }

    #[test]
    fn stdenv_anchor() {
        let stdenv = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-stdenv-linux.drv";
        let stage = "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-bootstrap-stage1-stdenv-linux.drv";
        let other = "/nix/store/cccccccccccccccccccccccccccccccc-stdenv-linux.drv";
        // app uses stdenv; tool (cached below) uses the other stdenv.
        let mut g = graph();
        g.derivations
            .get_mut("app.drv")
            .unwrap()
            .input_drvs
            .insert(stdenv.into(), ["out".to_string()].into());
        g.derivations
            .get_mut("tool.drv")
            .unwrap()
            .input_drvs
            .insert(other.into(), ["out".to_string()].into());
        g.derivations.insert(
            stdenv.into(),
            drv(&[("out", Some("stdenv-out"))], &[(stage, &["out"])], &[]),
        );
        g.derivations
            .insert(stage.into(), drv(&[("out", Some("stage-out"))], &[], &[]));
        g.derivations
            .insert(other.into(), drv(&[("out", Some("other-out"))], &[], &[]));
        let base = ["glibc", "openssl-dev", "deep", "src.tar"];
        // Cached stdenv: no warning (its bootstrap stages are never visited).
        let p = plan(&g, |p| base.contains(&p) || p == "stdenv-out" || p == "tool");
        assert!(anchor_warnings(&p).is_empty());
        // Uncached stdenv that is needed: one warning naming it. The other
        // stdenv is beneath a cached path (tool), so it is not needed.
        let p = plan(&g, |p| base.contains(&p) || p == "tool");
        let w = anchor_warnings(&p);
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].code, UNINDEXED_NIXPKGS);
        assert_eq!(w[0].paths, vec![stdenv.to_string()]);
        assert!(w[0].message.contains("stdenv-linux.drv"));
        // Both needed and uncached: both are named.
        let p = plan(&g, |p| base.contains(&p));
        assert_eq!(anchor_warnings(&p)[0].paths.len(), 2);
        // No stdenv in the graph: nothing to say.
        let p = plan(&graph(), |_| false);
        assert!(anchor_warnings(&p).is_empty());
    }

    #[test]
    fn close_misses() {
        let p = Plan {
            to_build: vec![
                "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-curl-8.22.0.drv".into(),
                "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-source.drv".into(),
                "/nix/store/cccccccccccccccccccccccccccccccc-myapp-1.0.drv".into(),
            ],
            ..Default::default()
        };
        let indexed = |name: &str| match name {
            "curl-8.22.0" => 11,
            "source" => 5000,
            _ => 0,
        };
        let w = close_miss_warnings(&p, indexed, DEFAULT_CLOSE_MISS_THRESHOLD);
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].code, CLOSE_MISS);
        assert_eq!(w[0].paths, vec![p.to_build[0].clone()]);
        assert!(
            w[0].message.contains("curl-8.22.0 (11 indexed)"),
            "{}",
            w[0].message
        );
        // A higher threshold includes generic names; 0 disables the check.
        assert_eq!(close_miss_warnings(&p, indexed, 10_000)[0].paths.len(), 2);
        assert!(close_miss_warnings(&p, indexed, 0).is_empty());
        assert!(close_miss_warnings(&p, indexed, 11).is_empty());
        assert_eq!(
            drv_name("/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-curl-8.22.0.drv"),
            "curl-8.22.0"
        );
    }

    #[test]
    fn roots_exclude_contained_hits() {
        let hits: BTreeSet<String> = ["a", "b", "c"].iter().map(|s| s.to_string()).collect();
        let edges: Vec<(String, String)> = [("a", "a"), ("a", "x"), ("x", "b"), ("c", "c")]
            .iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect();
        assert_eq!(roots(&hits, &edges), vec!["a", "c"]);
    }
}
