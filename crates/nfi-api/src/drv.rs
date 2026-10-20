//! Reduced derivation graphs, built from `nix derivation show --recursive`.

use anyhow::{Context, Result, bail};
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
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
    /// Fixed-output derivation (a fetch: its output hash is known up front).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub fixed: bool,
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

/// Nix's `makeStorePath`: `/nix/store/<nixbase32(compress(sha256(fingerprint), 20))>-<name>`.
fn make_store_path(kind: &str, inner_hex: &str, name: &str) -> String {
    let digest = Sha256::digest(format!("{kind}:sha256:{inner_hex}:/nix/store:{name}"));
    let mut compressed = [0u8; 20];
    for (i, b) in digest.iter().enumerate() {
        compressed[i % 20] ^= b;
    }
    format!(
        "{STORE}{}-{name}",
        nfi_interchange::nixbase32::encode(&compressed)
    )
}

/// The path of a fixed-output derivation's output, from its hash and method.
///
/// `nix derivation show` (Nix 2.34+) gives fixed outputs as
/// `{"hash": "sha256-<base64>", "method": "flat"|"nar"}` without a path, but
/// the path is fully determined by them (Nix's `makeFixedOutputPath`). The
/// `text` and `git` methods are not handled and yield `None`.
fn fixed_output_path(drv_path: &str, output: &str, o: &Value) -> Option<String> {
    let (algo, digest) = o.get("hash")?.as_str()?.split_once('-')?;
    let digest = base64::engine::general_purpose::STANDARD.decode(digest).ok()?;
    let digest_hex = hex::encode(digest);
    let drv_name = drv_path.rsplit('/').next()?.get(33..)?.strip_suffix(".drv")?;
    let name = if output == "out" {
        drv_name.to_string()
    } else {
        format!("{drv_name}-{output}")
    };
    match (o.get("method")?.as_str()?, algo) {
        ("nar", "sha256") => Some(make_store_path("source", &digest_hex, &name)),
        (method @ ("nar" | "flat"), _) => {
            let rec = if method == "nar" { "r:" } else { "" };
            let inner = Sha256::digest(format!("fixed:out:{rec}{algo}:{digest_hex}:"));
            Some(make_store_path("output:out", &hex::encode(inner), &name))
        }
        _ => None,
    }
}

fn reduce_one(drv_path: &str, d: &Value) -> Result<ReducedDrv> {
    let mut r = ReducedDrv::default();
    if let Some(fixed) = d.get("fixed").and_then(Value::as_bool) {
        // Already reduced.
        r.fixed = fixed;
    }
    if let Some(outs) = d.get("outputs").and_then(Value::as_object) {
        for (name, o) in outs {
            // Fixed outputs carry their hash (floating CA outputs only an
            // algorithm), in both the legacy and the current format.
            if o.get("hash")
                .and_then(Value::as_str)
                .is_some_and(|h| !h.is_empty())
            {
                r.fixed = true;
            }
            // Raw: {"path": "..."}, {"hash": ..., "method": ...} for fixed
            // outputs in newer Nix, or neither for floating CA outputs;
            // reduced: "..." or null.
            let path = match o {
                Value::String(s) => Some(full(s)),
                _ => o
                    .get("path")
                    .and_then(Value::as_str)
                    .map(full)
                    .or_else(|| fixed_output_path(drv_path, name, o)),
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
                reduce_one(path, d).with_context(|| format!("derivation {path}"))?,
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
    fn computes_fixed_output_paths() {
        // Outputs as `nix derivation show` (Nix 2.34.6) prints them; expected
        // paths from `nix-store -q --outputs` on the same derivations.
        let sha256 = "sha256-47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=";
        let sha1 = "sha1-EfatjsUqKYSrqv18O1FlA3hcIHI=";
        let sha512 =
            "sha512-pKvURIxJVi2CgRXROh/M6pJ/UrTVRZKX+LQ+QtqJI4vBNibkPcs43bCCSIkn7JBPtCBXRDmD6IWFF51QVRr+Yg==";
        let cases = [
            (
                "47sl882adlfgcfjpd47qc6zv9gj9fdqx-fod-flat.drv",
                sha256,
                "flat",
                "hl4id7pmbp11k4x1g3hq69mvvrfv6clf-fod-flat",
            ),
            (
                "znvd0qjx11y1g6769s405g5i0xfdgnd9-fod-recursive.drv",
                sha256,
                "nar",
                "wrj0w1nxhir7ls4br06c9h112i0l8pmb-fod-recursive",
            ),
            (
                "ph57bhh81412sdmk3f6sbhs251fq3c39-fod-sha1-flat.drv",
                sha1,
                "flat",
                "j1pbq9czp1sc5hxwan4l5xbrkbdyxp6d-fod-sha1-flat",
            ),
            (
                "l0nvs98jsplzjym1zapdw8wspib6rprq-fod-sha1-recursive.drv",
                sha1,
                "nar",
                "hmik9sn669rjpw56y0fax5wfr446d1cp-fod-sha1-recursive",
            ),
            (
                "wcnpq5j5ryjld5l7fl6cnqvr57s7w3kf-fod-sha512-flat.drv",
                sha512,
                "flat",
                "sdqizdmih7k9j93p9v095dc5h3np1f4l-fod-sha512-flat",
            ),
            (
                "1b8g4m3bbd4nm1fm8xk2lqqrhfprfw9v-fod-sha512-recursive.drv",
                sha512,
                "nar",
                "g771dnds6ga8hnckipqwv28nl09ixk6z-fod-sha512-recursive",
            ),
        ];
        let mut drvs = serde_json::Map::new();
        for (drv, hash, method, _) in cases {
            drvs.insert(
                drv.into(),
                serde_json::json!({"inputs": {"drvs": {}, "srcs": []},
                    "outputs": {"out": {"hash": hash, "method": method}}}),
            );
        }
        drvs.insert(
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-floating.drv".into(),
            serde_json::json!({"outputs": {"out": {"hashAlgo": "sha256", "method": "nar"}}}),
        );
        let g = ReducedGraph::from_json(&serde_json::json!({"derivations": drvs, "version": 4})).unwrap();
        let out = |drv: &str| g.derivations[&format!("/nix/store/{drv}")].outputs["out"].clone();
        for (drv, _, _, expected) in cases {
            assert_eq!(out(drv), Some(format!("/nix/store/{expected}")), "{drv}");
            assert!(g.derivations[&format!("/nix/store/{drv}")].fixed, "{drv}");
        }
        assert_eq!(out("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-floating.drv"), None);
        assert!(!g.derivations["/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-floating.drv"].fixed);
        // `fixed` survives the reduced form (what clients send).
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
        assert!(!x.fixed);
        assert_eq!(x.input_srcs.len(), 1);
        // Legacy fixed outputs: {"path", "hashAlgo", "hash"}.
        let v: Value = serde_json::from_str(
            r#"{"/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-src.tar.gz.drv":{"outputs":{"out":{"path":"/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-src.tar.gz","hashAlgo":"sha256","hash":"e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"}},"inputDrvs":{},"inputSrcs":[]}}"#,
        )
        .unwrap();
        assert!(
            ReducedGraph::from_json(&v)
                .unwrap()
                .derivations
                .values()
                .all(|d| d.fixed)
        );
        assert_eq!(g.all_paths().len(), 2);
    }
}
