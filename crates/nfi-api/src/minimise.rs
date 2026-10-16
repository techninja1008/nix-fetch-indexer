//! Minimise planning: which cached paths are useful for building a graph.

use crate::drv::ReducedGraph;
use std::collections::{BTreeMap, BTreeSet, HashSet};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    pub requested: Vec<String>,
    /// Derivations that must be built, in dependants-first order.
    pub to_build: Vec<String>,
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
        plan.to_build.push(drv.to_string());
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
        if reached.insert(n) {
            if let Some(next) = adj.get(n) {
                stack.extend(next.iter().copied());
            }
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
    fn roots_exclude_contained_hits() {
        let hits: BTreeSet<String> = ["a", "b", "c"].iter().map(|s| s.to_string()).collect();
        let edges: Vec<(String, String)> = [("a", "a"), ("a", "x"), ("x", "b"), ("c", "c")]
            .iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect();
        assert_eq!(roots(&hits, &edges), vec!["a", "c"]);
    }
}
