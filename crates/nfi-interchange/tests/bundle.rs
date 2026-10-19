use nfi_interchange::codec::{self, Compression, Format};
use nfi_interchange::ops::{self, ClosureBuilder, PathInfo};
use nfi_interchange::*;
use std::collections::BTreeMap;

fn h(n: u8) -> StoreHash {
    let mut b = [0u8; 20];
    b[0] = n;
    b[19] = n.wrapping_mul(7);
    StoreHash(b)
}

fn release(name: &str, commit: &str, channel: &str) -> Release {
    Release {
        name: name.into(),
        commit: commit.into(),
        channel: channel.into(),
        series: channel.trim_start_matches("nixos-").into(),
        url: None,
        datasets: Dataset::ALL.to_vec(),
    }
}

/// A bundle for one release with a small closure: 1 -> 2 -> 3, 4 -> 3.
fn sample(name: &str, commit: &str, extra: bool) -> Bundle {
    sample_typed(name, commit, extra, STORE_PATHS)
}

fn sample_typed(name: &str, commit: &str, extra: bool, closure_type: &str) -> Bundle {
    let mut b = Bundle::default();
    let r = b.ensure_release(release(name, commit, "nixos-25.05"));
    let mut cb = ClosureBuilder::new();
    let cbits = Bitset::single(cb.ensure_closure(r, closure_type));
    let bits = Bitset::single(r);
    let edges: &[(u8, &[u8])] = if extra {
        &[(1, &[2]), (2, &[3]), (4, &[3, 4]), (5, &[1])]
    } else {
        &[(1, &[2]), (2, &[3]), (4, &[3, 4])]
    };
    for &(n, refs) in edges {
        cb.insert(
            PathInfo {
                hash: h(n),
                name: format!("p{n}"),
                references: refs.iter().map(|&x| h(x)).collect(),
                nar_size: Some(n as u64 * 100),
                file_size: Some(n as u64 * 10),
                deriver: Some(format!("{}-p{n}.drv", h(n + 100))),
            },
            &cbits,
        );
    }
    cb.insert(
        PathInfo {
            hash: h(3),
            name: "p3".into(),
            references: vec![],
            nar_size: None,
            file_size: None,
            deriver: None,
        },
        &cbits,
    );
    cb.add_missing(h(9), "p9", &cbits);
    b.closure = Some(cb.finish());
    b.packages = Some(PackagesSection {
        entries: vec![Package {
            attr: "hello".into(),
            name: "hello-2.12".into(),
            pname: Some("hello".into()),
            version: Some("2.12".into()),
            system: "x86_64-linux".into(),
            description: Some("Say hello".into()),
            homepage: None,
            license: vec!["gpl3Plus".into()],
            main_program: Some("hello".into()),
            outputs: vec!["out".into()],
            broken: false,
            unfree: false,
            releases: bits.clone(),
        }],
    });
    b.outpaths = Some(OutpathsSection {
        entries: vec![Outpaths {
            attr: "hello".into(),
            system: "x86_64-linux".into(),
            drv_path: Some(format!("/nix/store/{}-hello.drv", h(101))),
            outputs: BTreeMap::from([("out".into(), format!("/nix/store/{}-p1", h(1)))]),
            releases: bits.clone(),
        }],
    });
    b.programs = Some(ProgramsSection {
        entries: vec![Program {
            name: "hello".into(),
            system: "x86_64-linux".into(),
            attr: "hello".into(),
            releases: bits,
        }],
    });
    b
}

#[test]
fn roundtrip_all_formats() {
    let b = sample("nixos-25.05.1.aaaa", "a".repeat(40).as_str(), false);
    b.validate().unwrap();
    for c in [
        Compression::None,
        Compression::Zstd(3),
        Compression::Xz(6),
        Compression::Brotli(5),
    ] {
        let mut buf = Vec::new();
        codec::write(&b, &mut buf, Format::Binary(c)).unwrap();
        assert_eq!(&buf[..4], b"NFIB");
        let (back, det) = codec::read_detect(&buf[..], STORE_PATHS).unwrap();
        assert_eq!(
            det,
            codec::Detected::Binary {
                compression_id: c.id()
            }
        );
        assert_eq!(back, b, "{c:?}");
    }
    let mut buf = Vec::new();
    codec::write(&b, &mut buf, Format::Json).unwrap();
    let text = String::from_utf8(buf.clone()).unwrap();
    assert!(!text.contains('\n'));
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["closure"]["paths"][0]["hash"].as_str().unwrap().len(), 32);
    let (back, det) = codec::read_detect(&buf[..], STORE_PATHS).unwrap();
    assert_eq!(det, codec::Detected::Json);
    assert_eq!(back, b);
}

#[test]
fn file_io() {
    let dir = tempfile::tempdir().unwrap();
    let b = sample("r1", &"a".repeat(40), false);
    let p = dir.path().join("x.nfib");
    codec::write_file(&b, &p, Format::Binary(Compression::DEFAULT)).unwrap();
    assert_eq!(codec::read_file(&p, STORE_PATHS).unwrap(), b);
}

#[test]
fn closure_builder_sorted_and_indexed() {
    let b = sample("r1", &"a".repeat(40), false);
    let c = b.closure.unwrap();
    assert_eq!(c.paths.len(), 4);
    assert!(c.paths.windows(2).all(|w| w[0].hash < w[1].hash));
    let p4 = c.paths.iter().find(|p| p.name == "p4").unwrap();
    let names: Vec<_> = p4
        .references
        .iter()
        .map(|&r| c.paths[r as usize].name.as_str())
        .collect();
    assert_eq!(names, vec!["p3", "p4"]);
    assert_eq!(c.missing.len(), 1);
}

#[test]
fn merge_and_split() {
    let a = sample("r1", &"a".repeat(40), false);
    let b = sample("r2", &"b".repeat(40), true);
    let m = ops::merge([a.clone(), b.clone()]);
    m.validate().unwrap();
    assert_eq!(m.releases.len(), 2);
    let c = m.closure.as_ref().unwrap();
    assert_eq!(c.paths.len(), 5);
    let p1 = c.paths.iter().find(|p| p.name == "p1").unwrap();
    assert_eq!(p1.closures.iter().collect::<Vec<_>>(), vec![0, 1]);
    let p5 = c.paths.iter().find(|p| p.name == "p5").unwrap();
    assert_eq!(p5.closures.iter().collect::<Vec<_>>(), vec![1]);
    // Identical package metadata deduplicates.
    assert_eq!(m.packages.as_ref().unwrap().entries.len(), 1);
    assert_eq!(m.packages.as_ref().unwrap().entries[0].releases.count(), 2);

    // Merge is idempotent.
    let mm = ops::merge([m.clone(), a.clone()]);
    assert_eq!(mm, m);

    let parts = ops::split(&m);
    assert_eq!(parts.len(), 2);
    assert_eq!(parts[0], ops::merge([a]));
    assert_eq!(parts[1], ops::merge([b]));
}

#[test]
fn filter_sections_and_keeps_referenced_paths() {
    let a = sample("r1", &"a".repeat(40), false);
    let b = sample("r2", &"b".repeat(40), true);
    let m = ops::merge([a, b]);
    let only_pk = ops::filter(&m, |r| r.name == "r1", Some(&[Dataset::Packages]), None);
    assert!(only_pk.closure.is_none() && only_pk.programs.is_none());
    assert_eq!(only_pk.releases[0].datasets, vec![Dataset::Packages]);
    assert_eq!(only_pk.packages.unwrap().entries.len(), 1);

    // Pretend p5 (only in r2) references a path with no release bits; it must be retained.
    let r2 = ops::filter(&m, |r| r.name == "r2", None, None);
    r2.validate().unwrap();
    assert_eq!(r2.closure.as_ref().unwrap().paths.len(), 5);
    let st = ops::stats(&r2);
    assert_eq!(st.releases[0].closure_paths, 5);
    assert_eq!(st.releases[0].missing_paths, 1);
}

#[test]
fn mark_closure_propagates() {
    let mut cb = ClosureBuilder::new();
    let none = Bitset::new();
    cb.insert(
        PathInfo {
            hash: h(1),
            name: "a".into(),
            references: vec![h(2)],
            nar_size: None,
            file_size: None,
            deriver: None,
        },
        &none,
    );
    cb.insert(
        PathInfo {
            hash: h(2),
            name: "b".into(),
            references: vec![h(3)],
            nar_size: None,
            file_size: None,
            deriver: None,
        },
        &none,
    );
    let unknown = cb.mark_closure([h(1)], 2);
    assert_eq!(unknown, vec![h(3)]);
    let c = cb.finish();
    assert!(c.paths.iter().all(|p| p.closures.get(2)));
}

#[test]
fn bench_runs() {
    let b = sample("r1", &"a".repeat(40), false);
    let r = codec::bench(&b, &[Compression::None, Compression::Zstd(1)]).unwrap();
    assert_eq!(r.len(), 2);
}

#[test]
fn rejects_garbage() {
    assert!(codec::read(&b"NOPE00"[..], STORE_PATHS).is_err());
    assert!(codec::read(&b"NFIB\x01\x09"[..], STORE_PATHS).is_err());
}

#[test]
fn validate_rejects_open_closures() {
    let mut b = sample("r1", &"a".repeat(40), false);
    b.validate().unwrap();
    let r2 = b.ensure_release(release("r2", &"b".repeat(40), "nixos-25.05"));
    // p1 (which references p2) joins r2 without p2: the closure is not closed.
    let c = b.closure.as_mut().unwrap();
    let c2 = c.ensure_closure(r2 as u32, STORE_PATHS);
    let p1 = c.paths.iter_mut().find(|p| p.name == "p1").unwrap();
    p1.closures.set(c2);
    let err = b.validate().unwrap_err();
    assert!(err.contains("p1") && err.contains("p2"), "{err}");
    assert!(Bitset::from_indices([1, 9]).is_subset(&Bitset::from_indices([1, 2, 9])));
    assert!(!Bitset::from_indices([1, 9]).is_subset(&Bitset::from_indices([1])));
    assert!(Bitset::new().is_subset(&Bitset::new()));
}

#[test]
fn v2_bundles_upgrade_with_the_given_closure_type() {
    // A version 2 bundle: no closure table, closure bits are release indices.
    let mut v2 = sample("r1", &"a".repeat(40), false);
    let r2 = v2.ensure_release(release("r2", &"b".repeat(40), "nixos-25.05"));
    v2.releases[r2].datasets.retain(|d| *d != Dataset::Closure);
    v2.format_version = 2;
    v2.closure.as_mut().unwrap().closures.clear();
    let mut buf = Vec::new();
    codec::write(&v2, &mut buf, Format::Json).unwrap();
    let text = String::from_utf8(buf)
        .unwrap()
        .replace("\"closures\":", "\"releases\":");
    let back = codec::read(text.as_bytes(), "helpers-direct").unwrap();
    back.validate().unwrap();
    assert_eq!(back.format_version, FORMAT_VERSION);
    let c = back.closure.as_ref().unwrap();
    // Only releases with the closure dataset get a closure.
    assert_eq!(
        c.closures,
        vec![Closure {
            release: 0,
            closure_type: "helpers-direct".into(),
            skipped: vec![],
        }]
    );
    assert!(
        c.paths
            .iter()
            .all(|p| p.closures.iter().collect::<Vec<_>>() == [0])
    );
    // Unupgraded version 2 bundles are rejected.
    assert!(v2.validate().unwrap_err().contains("format version 2"));
}

#[test]
fn closure_types_merge_and_filter() {
    let rt = sample("r1", &"a".repeat(40), false);
    let helpers = sample_typed("r1", &"a".repeat(40), true, "helpers-direct");
    let m = ops::merge([rt.clone(), helpers]);
    m.validate().unwrap();
    assert_eq!(m.releases.len(), 1);
    let c = m.closure.as_ref().unwrap();
    let types: Vec<_> = c.closures.iter().map(|c| c.closure_type.as_str()).collect();
    assert_eq!(types, ["store-paths", "helpers-direct"]);
    let p1 = c.paths.iter().find(|p| p.name == "p1").unwrap();
    assert_eq!(p1.closures.iter().collect::<Vec<_>>(), vec![0, 1]);
    let p5 = c.paths.iter().find(|p| p.name == "p5").unwrap();
    assert_eq!(p5.closures.iter().collect::<Vec<_>>(), vec![1]);
    let st = ops::stats(&m);
    assert_eq!(st.releases[0].closure_paths, 5);
    assert_eq!(
        st.closures.iter().map(|c| c.paths).collect::<Vec<_>>(),
        vec![4, 5]
    );

    // Filtering by closure type keeps only that closure.
    let only_rt = ops::filter(&m, |_| true, None, Some(&["store-paths".to_string()]));
    only_rt.validate().unwrap();
    assert_eq!(ops::merge([only_rt.clone()]), ops::merge([rt]));
    // A release left without closures loses the closure dataset.
    let none = ops::filter(&m, |_| true, None, Some(&["eval".to_string()]));
    none.validate().unwrap();
    assert!(!none.releases[0].datasets.contains(&Dataset::Closure));
    assert!(none.closure.as_ref().unwrap().paths.is_empty());
}

#[test]
fn validate_rejects_closures_of_releases_without_the_dataset() {
    let mut b = sample("r1", &"a".repeat(40), false);
    b.releases[0].datasets.retain(|d| *d != Dataset::Closure);
    assert!(b.validate().unwrap_err().contains("lacks the closure dataset"));
}
