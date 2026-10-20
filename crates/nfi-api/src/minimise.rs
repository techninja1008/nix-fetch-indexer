//! Minimise planning: which cached paths are useful for building a graph.

use crate::drv::{ReducedDrv, ReducedGraph};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

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
    Planner::new(graph).run(|p| Some(is_cached(p))).0
}

/// [`plan`] with the cache looked up in rounds, so that only the paths the
/// plan consults are looked up: nothing beneath a hit is.
///
/// Each round, [`Planner::step`] plans with what is known so far, treating a
/// derivation whose needed outputs are not known yet as a hit, and returns
/// the paths it consulted that are not known. Once none are, the plan only
/// depends on known paths, so it is the plan [`plan`] gives.
pub struct Planner<'g> {
    graph: &'g ReducedGraph,
    requested: Vec<String>,
    /// Positions of the requested derivations in `nodes`.
    roots: Vec<usize>,
    /// Every derivation reachable from the requested ones, dependants first.
    nodes: Vec<Node<'g>>,
    known: HashMap<String, bool>,
}

struct Node<'g> {
    path: &'g str,
    /// `None` for an input derivation missing from the graph.
    drv: Option<&'g ReducedDrv>,
    /// Input derivations by position in `nodes`, with the outputs consumed.
    inputs: Vec<(usize, &'g BTreeSet<String>)>,
}

impl<'g> Planner<'g> {
    pub fn new(graph: &'g ReducedGraph) -> Self {
        // Number the derivations, and missing input derivations after them.
        let mut paths: Vec<&str> = graph.derivations.keys().map(String::as_str).collect();
        let mut index: HashMap<&str, usize> = paths.iter().enumerate().map(|(i, &p)| (p, i)).collect();
        let mut referenced = vec![false; paths.len()];
        let mut inputs: Vec<Vec<(usize, &BTreeSet<String>)>> = Vec::with_capacity(paths.len());
        for d in graph.derivations.values() {
            let mut ins = Vec::with_capacity(d.input_drvs.len());
            for (input, outs) in &d.input_drvs {
                let i = *index.entry(input.as_str()).or_insert_with(|| {
                    paths.push(input.as_str());
                    paths.len() - 1
                });
                if let Some(r) = referenced.get_mut(i) {
                    *r = true;
                }
                ins.push((i, outs));
            }
            inputs.push(ins);
        }
        inputs.resize_with(paths.len(), Vec::new);
        let roots: Vec<usize> = (0..graph.derivations.len()).filter(|&i| !referenced[i]).collect();
        let order = topo_order(&inputs, &roots);
        let mut position = vec![usize::MAX; paths.len()];
        for (pos, &i) in order.iter().enumerate() {
            position[i] = pos;
        }
        let nodes = order
            .iter()
            .map(|&i| Node {
                path: paths[i],
                drv: graph.derivations.get(paths[i]),
                inputs: inputs[i].iter().map(|&(j, outs)| (position[j], outs)).collect(),
            })
            .collect();
        Planner {
            graph,
            requested: roots.iter().map(|&i| paths[i].to_string()).collect(),
            roots: roots.iter().map(|&i| position[i]).collect(),
            nodes,
            known: HashMap::new(),
        }
    }

    /// Record a lookup: each of `queried` is cached if it is in `cached`.
    pub fn learn<'a>(
        &mut self,
        queried: impl IntoIterator<Item = &'a String>,
        cached: &impl Fn(&str) -> bool,
    ) {
        for p in queried {
            self.known.insert(p.clone(), cached(p));
        }
    }

    /// The paths of the graph that are not known yet.
    pub fn remaining(&self) -> BTreeSet<String> {
        let mut s = self.graph.all_paths();
        s.retain(|p| !self.known.contains_key(p));
        s
    }

    /// The plan so far, and the paths it consulted that are not known. The
    /// plan is final when there are none.
    pub fn step(&self) -> (Plan, BTreeSet<String>) {
        self.run(|p| self.known.get(p).copied())
    }

    fn run(&self, is_cached: impl Fn(&str) -> Option<bool>) -> (Plan, BTreeSet<String>) {
        let mut needed: Vec<Option<BTreeSet<&str>>> = vec![None; self.nodes.len()];
        for &r in &self.roots {
            needed[r] = self.nodes[r]
                .drv
                .map(|d| d.outputs.keys().map(String::as_str).collect());
        }
        let mut plan = Plan {
            requested: self.requested.clone(),
            ..Default::default()
        };
        let mut unknown = BTreeSet::new();
        let mut unresolved = BTreeSet::new();
        for (i, node) in self.nodes.iter().enumerate() {
            let Some(outs) = needed[i].take() else { continue };
            let Some(d) = node.drv else {
                unresolved.insert(node.path.to_string());
                continue;
            };
            let paths: Option<Vec<&String>> = outs
                .iter()
                .map(|&o| d.outputs.get(o).and_then(|p| p.as_ref()))
                .collect();
            if let Some(paths) = paths.filter(|ps| !ps.is_empty()) {
                let cached: Vec<Option<bool>> = paths.iter().map(|p| is_cached(p)).collect();
                // One output known to be uncached decides it, whatever the
                // others are; otherwise unknown outputs are taken as cached.
                if !cached.contains(&Some(false)) {
                    for (p, c) in paths.into_iter().zip(cached) {
                        if c.is_none() {
                            unknown.insert(p.clone());
                        }
                        plan.hits.insert(p.clone());
                    }
                    continue;
                }
            }
            if d.fixed {
                plan.to_fetch.push(node.path.to_string());
            } else {
                plan.to_build.push(node.path.to_string());
            }
            for &(j, input_outs) in &node.inputs {
                needed[j]
                    .get_or_insert_default()
                    .extend(input_outs.iter().map(String::as_str));
                if self.nodes[j].drv.is_none() {
                    unresolved.insert(self.nodes[j].path.to_string());
                }
            }
            for src in &d.input_srcs {
                match is_cached(src) {
                    Some(true) => {
                        plan.hits.insert(src.clone());
                    }
                    Some(false) => {}
                    None => {
                        unknown.insert(src.clone());
                    }
                }
            }
        }
        plan.unresolved = unresolved.into_iter().collect();
        (plan, unknown)
    }
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

/// Dependants-first order of every node reachable from `roots` (reverse DFS
/// post-order).
fn topo_order(inputs: &[Vec<(usize, &BTreeSet<String>)>], roots: &[usize]) -> Vec<usize> {
    let mut post = Vec::new();
    let mut visited = vec![false; inputs.len()];
    for &root in roots {
        if std::mem::replace(&mut visited[root], true) {
            continue;
        }
        // Stack of (node, inputs left to visit, last first).
        let mut stack = vec![(root, inputs[root].iter().rev())];
        while let Some((node, children)) = stack.last_mut() {
            if let Some(&(c, _)) = children.next() {
                if !std::mem::replace(&mut visited[c], true) {
                    stack.push((c, inputs[c].iter().rev()));
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
    fn several_requested() {
        // A second requested derivation that shares tool with app.
        let mut g = graph();
        g.derivations.insert(
            "zapp.drv".into(),
            drv(
                &[("out", Some("zapp")), ("doc", Some("zapp-doc"))],
                &[("tool.drv", &["out"])],
                &[],
            ),
        );
        let p = plan(&g, |p| p == "tool");
        assert_eq!(p.requested, vec!["app.drv", "zapp.drv"]);
        assert!(p.to_build.contains(&"zapp.drv".to_string()));
        assert!(p.to_build.contains(&"app.drv".to_string()));
        assert!(p.hits.contains("tool"));
        // Every output of a requested derivation is needed.
        let p = plan(&g, |p| p != "zapp-doc");
        assert_eq!(p.to_build, vec!["zapp.drv"]);
        assert_eq!(p.hits.into_iter().collect::<Vec<_>>(), vec!["app", "tool"]);
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

    /// Plan with a [`Planner`], returning the plan and every path looked up.
    fn plan_in_rounds(g: &ReducedGraph, is_cached: impl Fn(&str) -> bool) -> (Plan, BTreeSet<String>) {
        let mut planner = Planner::new(g);
        let mut looked_up = BTreeSet::new();
        loop {
            let (plan, unknown) = planner.step();
            if unknown.is_empty() {
                return (plan, looked_up);
            }
            assert!(unknown.is_disjoint(&looked_up), "looked up again: {unknown:?}");
            planner.learn(&unknown, &is_cached);
            looked_up.extend(unknown);
        }
    }

    #[test]
    fn rounds_match_plan() {
        let mut fetching = graph();
        fetching
            .derivations
            .get_mut("lib.drv")
            .unwrap()
            .input_drvs
            .insert("src.drv".into(), ["out".to_string()].into());
        let mut src = drv(&[("out", Some("src"))], &[("curl.drv", &["bin"])], &[]);
        src.fixed = true;
        fetching.derivations.insert("src.drv".into(), src);
        fetching
            .derivations
            .insert("curl.drv".into(), drv(&[("bin", Some("curl-bin"))], &[], &[]));
        let mut several = graph();
        several.derivations.insert(
            "zapp.drv".into(),
            drv(
                &[("out", Some("zapp"))],
                &[("openssl.drv", &["out"])],
                &["src.tar"],
            ),
        );
        for g in [graph(), fetching, several] {
            let paths: Vec<String> = g.all_paths().into_iter().collect();
            for mask in 0..1u32 << paths.len() {
                let cached = |p: &str| {
                    paths
                        .iter()
                        .position(|q| q == p)
                        .is_some_and(|i| mask & 1 << i != 0)
                };
                let (p, _) = plan_in_rounds(&g, cached);
                assert_eq!(p, plan(&g, cached), "cached mask {mask:b} of {paths:?}");
            }
        }
    }

    #[test]
    fn rounds_skip_beneath_hits() {
        let cached = ["glibc", "openssl-dev", "tool", "deep", "src.tar", "bootstrap"];
        let (p, looked_up) = plan_in_rounds(&graph(), |p| cached.contains(&p));
        assert_eq!(p, plan(&graph(), |p| cached.contains(&p)));
        // Beneath the hits glibc, openssl-dev and tool: not looked up.
        for p in ["bootstrap", "deep", "openssl"] {
            assert!(!looked_up.contains(p), "{p} looked up");
        }
        // Everything cached: only the requested outputs.
        let (_, looked_up) = plan_in_rounds(&graph(), |_| true);
        assert_eq!(looked_up.into_iter().collect::<Vec<_>>(), vec!["app"]);
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
