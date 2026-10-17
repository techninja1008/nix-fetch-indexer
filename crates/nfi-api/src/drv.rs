//! Reduced derivation graphs, built from `nix derivation show --recursive`.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

const STORE: &str = "/nix/store/";

/// A derivation reduced to the parts needed for dependency planning.
/// All paths are full `/nix/store/...` paths.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReducedDrv {
    /// Output name to path (`None` for content-addressed/floating outputs).
    pub outputs: BTreeMap<String, Option<String>>,
    /// Input derivation to the output names consumed from it.
    #[serde(default, rename = "inputDrvs")]
    pub input_drvs: BTreeMap<String, BTreeSet<String>>,
    #[serde(default, rename = "inputSrcs")]
    pub input_srcs: BTreeSet<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReducedGraph {
    pub derivations: BTreeMap<String, ReducedDrv>,
}

fn full(p: &str) -> String {
    if p.starts_with('/') {
        p.to_string()
    } else {
        format!("{STORE}{p}")
    }
}

fn output_names(v: &Value) -> Result<BTreeSet<String>> {
    // Legacy: ["out", "dev"]. Newer: {"outputs": [...], "dynamicOutputs": {...}}.
    let arr = match v {
        Value::Array(_) => v,
        Value::Object(o) => o.get("outputs").context("inputDrv without outputs")?,
        _ => bail!("unexpected inputDrv value {v}"),
    };
    Ok(arr
        .as_array()
        .context("inputDrv outputs not a list")?
        .iter()
        .filter_map(|x| x.as_str().map(str::to_string))
        .collect())
}

fn reduce_one(d: &Value) -> Result<ReducedDrv> {
    let mut r = ReducedDrv::default();
    if let Some(outs) = d.get("outputs").and_then(Value::as_object) {
        for (name, o) in outs {
            // Raw: {"path": "..."} (absent for floating CA outputs); reduced: "..." or null.
            let path = match o {
                Value::String(s) => Some(full(s)),
                _ => o.get("path").and_then(Value::as_str).map(full),
            };
            r.outputs.insert(name.clone(), path);
        }
    }
    let (drvs, srcs) = match d.get("inputs") {
        Some(inputs) => (inputs.get("drvs"), inputs.get("srcs")),
        None => (d.get("inputDrvs"), d.get("inputSrcs")),
    };
    if let Some(drvs) = drvs.and_then(Value::as_object) {
        for (p, v) in drvs {
            r.input_drvs.insert(full(p), output_names(v)?);
        }
    }
    if let Some(srcs) = srcs.and_then(Value::as_array) {
        r.input_srcs = srcs.iter().filter_map(|s| s.as_str().map(full)).collect();
    }
    Ok(r)
}

impl ReducedGraph {
    /// Accept either raw `nix derivation show` output (legacy map-of-paths
    /// format or the `{"derivations": ..., "version": N}` format) or an
    /// already reduced graph.
    pub fn from_json(v: &Value) -> Result<Self> {
        let obj = v.as_object().context("expected a JSON object")?;
        let map = match obj.get("derivations").and_then(Value::as_object) {
            Some(m) => m,
            None => obj,
        };
        let mut g = ReducedGraph::default();
        for (path, d) in map {
            if !path.ends_with(".drv") {
                continue;
            }
            g.derivations.insert(
                full(path),
                reduce_one(d).with_context(|| format!("derivation {path}"))?,
            );
        }
        Ok(g)
    }

    pub fn merge(&mut self, other: ReducedGraph) {
        self.derivations.extend(other.derivations);
    }

    /// Derivations that no other derivation in the graph depends on.
    pub fn requested(&self) -> Vec<String> {
        let referenced: BTreeSet<&String> = self
            .derivations
            .values()
            .flat_map(|d| d.input_drvs.keys())
            .collect();
        self.derivations
            .keys()
            .filter(|k| !referenced.contains(k))
            .cloned()
            .collect()
    }

    /// All store paths mentioned as outputs or input sources.
    pub fn all_paths(&self) -> BTreeSet<String> {
        let mut s = BTreeSet::new();
        for d in self.derivations.values() {
            s.extend(d.outputs.values().flatten().cloned());
            s.extend(d.input_srcs.iter().cloned());
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reduces_v4_format() {
        let v: Value = serde_json::from_str(
            r#"{"derivations":{
          "0kchj5r1l1i7k32mlwybj099flmqnw4g-b.drv":{"env":{"x":"y"},"inputs":{"drvs":{"mq9yfr049mwif1agr68la1sgsxw8kqpv-a.drv":{"dynamicOutputs":{},"outputs":["lib"]}},"srcs":["84m8sr66n6j8ygyxbky7js67k2zjqw03-srcf.txt"]},
             "outputs":{"dev":{"path":"q01hzy6ab22pnf3bzz4nf3jsdl5yxi5m-b-dev"},"out":{"path":"pz71rggm46l6zw8m2y5d841k91zrhrlj-b"}}},
          "mq9yfr049mwif1agr68la1sgsxw8kqpv-a.drv":{"inputs":{"drvs":{},"srcs":[]},
             "outputs":{"lib":{"path":"ivdvygqpvh3137gmd9pgrfajc0m7swiv-a-lib"},"out":{"path":"7ibag1g0clmswbbj6dx9y1yy28rj3hjh-a"}}}},
          "version":4}"#,
        )
        .unwrap();
        let g = ReducedGraph::from_json(&v).unwrap();
        assert_eq!(g.derivations.len(), 2);
        let b = &g.derivations["/nix/store/0kchj5r1l1i7k32mlwybj099flmqnw4g-b.drv"];
        assert_eq!(
            b.outputs["out"].as_deref(),
            Some("/nix/store/pz71rggm46l6zw8m2y5d841k91zrhrlj-b")
        );
        assert_eq!(
            b.input_drvs["/nix/store/mq9yfr049mwif1agr68la1sgsxw8kqpv-a.drv"],
            BTreeSet::from(["lib".to_string()])
        );
        assert_eq!(
            g.requested(),
            vec!["/nix/store/0kchj5r1l1i7k32mlwybj099flmqnw4g-b.drv"]
        );
        // Reduced form round-trips through from_json.
        let again = ReducedGraph::from_json(&serde_json::to_value(&g).unwrap()).unwrap();
        assert_eq!(again, g);
    }

    #[test]
    fn reduces_legacy_format() {
        let v: Value = serde_json::from_str(
            r#"{"/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-x.drv":{"outputs":{"out":{"path":"/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-x"},"ca":{}},
            "inputDrvs":{"/nix/store/cccccccccccccccccccccccccccccccc-y.drv":["out"]},"inputSrcs":["/nix/store/dddddddddddddddddddddddddddddddd-s"]}}"#,
        )
        .unwrap();
        let g = ReducedGraph::from_json(&v).unwrap();
        let x = &g.derivations["/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-x.drv"];
        assert_eq!(x.outputs["ca"], None);
        assert_eq!(x.input_srcs.len(), 1);
        assert_eq!(g.all_paths().len(), 2);
    }
}
