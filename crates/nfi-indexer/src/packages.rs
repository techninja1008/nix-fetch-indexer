//! Packages indexer: packages.json(.br).

use anyhow::{Context, Result};
use nfi_interchange::{Bitset, Package};
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::Read;

#[derive(Deserialize)]
struct PackagesJson {
    #[serde(default)]
    packages: BTreeMap<String, RawPackage>,
}

#[derive(Deserialize)]
struct RawPackage {
    name: String,
    #[serde(default)]
    pname: Option<String>,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    system: Option<String>,
    #[serde(default)]
    outputs: Option<BTreeMap<String, Value>>,
    #[serde(default)]
    meta: RawMeta,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct RawMeta {
    #[serde(default)]
    description: Option<Value>,
    #[serde(default)]
    homepage: Option<Value>,
    #[serde(default)]
    license: Option<Value>,
    #[serde(default)]
    main_program: Option<Value>,
    #[serde(default)]
    broken: Option<Value>,
    #[serde(default)]
    unfree: Option<Value>,
}

fn as_string(v: &Option<Value>) -> Option<String> {
    match v {
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        Some(Value::Array(a)) => a.iter().find_map(|x| x.as_str().map(str::to_string)),
        _ => None,
    }
}

fn as_bool(v: &Option<Value>) -> bool {
    matches!(v, Some(Value::Bool(true)))
}

fn license_names(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::String(s) => out.push(s.clone()),
        Value::Array(a) => a.iter().for_each(|x| license_names(x, out)),
        Value::Object(o) => {
            if let Some(s) = ["spdxId", "shortName", "fullName"]
                .iter()
                .find_map(|k| o.get(*k).and_then(Value::as_str))
            {
                out.push(s.to_string());
            }
        }
        _ => {}
    }
}

fn unfree(meta: &RawMeta) -> bool {
    if as_bool(&meta.unfree) {
        return true;
    }
    fn walk(v: &Value) -> bool {
        match v {
            Value::Array(a) => a.iter().any(walk),
            Value::Object(o) => o.get("free").is_some_and(|f| f == &Value::Bool(false)),
            _ => false,
        }
    }
    meta.license.as_ref().is_some_and(walk)
}

/// Parse packages.json (uncompressed) into package entries for one release.
pub fn parse_packages_json<R: Read>(r: R, release: usize) -> Result<Vec<Package>> {
    let pj: PackagesJson =
        serde_json::from_reader(std::io::BufReader::new(r)).context("parsing packages.json")?;
    let bits = Bitset::single(release);
    let mut out = Vec::with_capacity(pj.packages.len());
    for (attr, p) in pj.packages {
        let mut license = Vec::new();
        if let Some(l) = &p.meta.license {
            license_names(l, &mut license);
        }
        license.dedup();
        out.push(Package {
            attr,
            pname: p.pname,
            version: p.version.filter(|v| !v.is_empty()),
            system: p.system.unwrap_or_else(|| "x86_64-linux".into()),
            description: as_string(&p.meta.description),
            homepage: as_string(&p.meta.homepage),
            license,
            main_program: as_string(&p.meta.main_program),
            outputs: p.outputs.map(|o| o.into_keys().collect()).unwrap_or_default(),
            broken: as_bool(&p.meta.broken),
            unfree: unfree(&p.meta),
            name: p.name,
            releases: bits.clone(),
        });
    }
    Ok(out)
}

/// Parse brotli-compressed packages.json.br.
pub fn parse_packages_json_br(data: &[u8], release: usize) -> Result<Vec<Package>> {
    parse_packages_json(brotli::Decompressor::new(data, 1 << 16), release)
}

#[cfg(test)]
mod tests {
    #[test]
    fn parses() {
        let j = r#"{"version":"2","packages":{
          "hello":{"name":"hello-2.12","pname":"hello","version":"2.12","system":"x86_64-linux","outputName":"out",
                   "outputs":{"out":null},"meta":{"description":"Hi","homepage":["https://h"],"mainProgram":"hello",
                   "license":{"spdxId":"GPL-3.0-or-later","free":true}}},
          "steam":{"name":"steam-1","system":"x86_64-linux","meta":{"license":[{"shortName":"unfree","free":false}],"broken":true}}
        }}"#;
        let p = super::parse_packages_json(j.as_bytes(), 0).unwrap();
        assert_eq!(p.len(), 2);
        assert_eq!(p[0].attr, "hello");
        assert_eq!(p[0].homepage.as_deref(), Some("https://h"));
        assert_eq!(p[0].license, vec!["GPL-3.0-or-later"]);
        assert_eq!(p[0].outputs, vec!["out"]);
        assert!(!p[0].unfree);
        assert!(p[1].unfree && p[1].broken);
        assert!(p[1].outputs.is_empty());
    }
}
