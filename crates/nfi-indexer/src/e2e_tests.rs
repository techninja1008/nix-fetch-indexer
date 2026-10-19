//! End-to-end indexer tests against a local fixture HTTP server.

use crate::closure::ClosureOptions;
use crate::fetch::Fetcher;
use crate::index::{IndexOptions, index_releases};
use crate::outpaths::OutpathsOptions;
use crate::probes::{HELPER_PROBES, HELPERS_DIRECT, HELPERS_FULL, ProbeOptions};
use crate::release::{self, ReleaseLocation};
use nfi_interchange::*;
use std::io::Write;
use std::path::Path;

pub(crate) fn hash(n: u32) -> String {
    let mut b = [0u8; 20];
    b[..4].copy_from_slice(&n.to_le_bytes());
    StoreHash(b).to_string()
}

fn xz(data: &[u8]) -> Vec<u8> {
    let mut e = xz2::write::XzEncoder::new(Vec::new(), 1);
    e.write_all(data).unwrap();
    e.finish().unwrap()
}

fn br(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut w = brotli::CompressorWriter::new(&mut out, 4096, 5, 22);
        w.write_all(data).unwrap();
    }
    out
}

/// Fixture: paths 1..=5; 1 -> 2 -> 3; 4 -> 3; 5 -> 1. Release A lists 1, 4 and
/// a missing path 99; release B lists 1, 5.
fn write_fixture(root: &Path) {
    let cache = root.join("cache");
    std::fs::create_dir_all(&cache).unwrap();
    let refs: &[(u32, &[u32])] = &[(1, &[1, 2]), (2, &[3]), (3, &[]), (4, &[3]), (5, &[1])];
    for (n, r) in refs {
        let refs: Vec<String> = r.iter().map(|x| format!("{}-p{x}", hash(*x))).collect();
        let body = format!(
            "StorePath: /nix/store/{}-p{n}\nURL: nar/x.nar.xz\nCompression: xz\nFileSize: {}\nNarSize: {}\nReferences: {}\nDeriver: {}-p{n}.drv\n",
            hash(*n),
            n * 10,
            n * 100,
            refs.join(" "),
            hash(n + 1000)
        );
        std::fs::write(cache.join(format!("{}.narinfo", hash(*n))), body).unwrap();
    }
    for (name, roots) in [
        ("nixos-25.05.100.aaaaaaaaaaaa", vec![1u32, 4, 99]),
        ("nixos-25.05.200.bbbbbbbbbbbb", vec![1, 5]),
    ] {
        let dir = root.join("releases/nixos/25.05").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let c = if name.contains("aaa") { 'a' } else { 'b' };
        std::fs::write(dir.join("git-revision"), c.to_string().repeat(40)).unwrap();
        let sp: String = roots
            .iter()
            .map(|n| format!("/nix/store/{}-p{n}\n", hash(*n)))
            .collect();
        std::fs::write(dir.join("store-paths.xz"), xz(sp.as_bytes())).unwrap();
        let pj = format!(
            r#"{{"version":"2","packages":{{"hello":{{"name":"hello-{c}","pname":"hello","version":"{c}","system":"x86_64-linux","outputs":{{"out":null}},"meta":{{"description":"greeter","mainProgram":"hello"}}}}}}}}"#
        );
        std::fs::write(dir.join("packages.json.br"), br(pj.as_bytes())).unwrap();
        let db = crate::programs::make_programs_db(&[("hello", "x86_64-linux", "hello")]);
        let tar = crate::tarball::make_tar_xz(&[
            (&format!("{name}/default.nix"), b"{}"),
            (&format!("{name}/programs.sqlite"), &db),
        ]);
        std::fs::write(dir.join("nixexprs.tar.xz"), tar).unwrap();
    }
    let list = r#"<ListBucketResult><IsTruncated>false</IsTruncated>
<CommonPrefixes><Prefix>nixos/25.05/nixos-25.05.200.bbbbbbbbbbbb/</Prefix></CommonPrefixes>
<CommonPrefixes><Prefix>nixos/25.05/nixos-25.05.100.aaaaaaaaaaaa/</Prefix></CommonPrefixes></ListBucketResult>"#;
    std::fs::create_dir_all(root.join("s3")).unwrap();
    std::fs::write(root.join("s3/index.xml"), list).unwrap();
}

async fn serve(root: &Path) -> String {
    use axum::Router;
    use tower_http::services::{ServeDir, ServeFile};
    let app = Router::new()
        .nest_service("/releases", ServeDir::new(root.join("releases")))
        .nest_service("/cache", ServeDir::new(root.join("cache")))
        .route_service("/s3/", ServeFile::new(root.join("s3/index.xml")));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    format!("http://{addr}")
}

fn opts(base: &str, datasets: Vec<Dataset>, eval: &Path) -> IndexOptions {
    IndexOptions {
        datasets,
        closure_types: vec![STORE_PATHS.into()],
        closure: ClosureOptions {
            cache_url: format!("{base}/cache"),
            concurrency: 4,
            sample: None,
        },
        probes: ProbeOptions::default(),
        outpaths: OutpathsOptions {
            command: vec![eval.display().to_string()],
            systems: vec!["x86_64-linux".into()],
            workers: 1,
            max_memory_mb: 100,
        },
        include_seed: false,
    }
}

fn fake_eval(dir: &Path) -> std::path::PathBuf {
    let p = dir.join("fake-nix-eval-jobs");
    let line = format!(
        r#"{{"attr":"hello","attrPath":["hello"],"drvPath":"/nix/store/{}-hello.drv","outputs":{{"out":"/nix/store/{}-p1"}},"system":"x86_64-linux"}}"#,
        hash(1001),
        hash(1)
    );
    let script = format!(
        "#!/bin/sh\ncase \"$*\" in *--expr*) ;; *) exit 3;; esac\necho '{line}'\necho '{{\"attr\":\"broken\",\"error\":\"nope\"}}'\n"
    );
    std::fs::write(&p, script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

fn names(c: &ClosureSection, release: usize) -> Vec<String> {
    typed_names(c, release, STORE_PATHS)
}

fn typed_names(c: &ClosureSection, release: usize, closure_type: &str) -> Vec<String> {
    let ci = c
        .closure_index(release as u32, closure_type)
        .expect("closure exists");
    let mut v: Vec<String> = c
        .paths
        .iter()
        .filter(|p| p.closures.get(ci))
        .map(|p| p.name.clone())
        .collect();
    v.sort();
    v
}

#[tokio::test]
async fn index_two_releases_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    write_fixture(dir.path());
    let eval = fake_eval(dir.path());
    let base = serve(dir.path()).await;
    let fetcher = Fetcher::new(1).unwrap();
    let locs = release::list_channel(
        &fetcher,
        &format!("{base}/s3"),
        &format!("{base}/releases"),
        "nixos/25.05",
    )
    .await
    .unwrap();
    assert_eq!(
        locs.iter().map(|l| l.name.as_str()).collect::<Vec<_>>(),
        vec!["nixos-25.05.100.aaaaaaaaaaaa", "nixos-25.05.200.bbbbbbbbbbbb"]
    );
    // Everything that does not need Nix (eval errors come from helper
    // probes; see helper_probe_closures).
    let datasets: Vec<Dataset> = Dataset::ALL
        .into_iter()
        .filter(|d| *d != Dataset::EvalErrors)
        .collect();
    let b = index_releases(&fetcher, &locs, &opts(&base, datasets.clone(), &eval), None)
        .await
        .unwrap();
    b.validate().unwrap();
    assert_eq!(b.releases.len(), 2);
    assert_eq!(b.releases[0].commit, "a".repeat(40));
    assert_eq!(b.releases[0].channel, "nixos-25.05");
    assert_eq!(b.releases[0].datasets, datasets);
    let c = b.closure.as_ref().unwrap();
    assert_eq!(names(c, 0), vec!["p1", "p2", "p3", "p4"]);
    assert_eq!(names(c, 1), vec!["p1", "p2", "p3", "p5"]);
    assert_eq!(c.missing.len(), 1);
    let (c0, c1) = (
        c.closure_index(0, STORE_PATHS).unwrap(),
        c.closure_index(1, STORE_PATHS).unwrap(),
    );
    assert!(c.missing[0].closures.get(c0) && !c.missing[0].closures.get(c1));
    let p1 = c.paths.iter().find(|p| p.name == "p1").unwrap();
    assert_eq!(p1.nar_size, Some(100));
    assert_eq!(p1.references.len(), 2);
    assert_eq!(b.packages.as_ref().unwrap().entries.len(), 2);
    assert_eq!(b.programs.as_ref().unwrap().entries.len(), 1);
    assert_eq!(b.programs.as_ref().unwrap().entries[0].releases.count(), 2);
    let op = &b.outpaths.as_ref().unwrap().entries;
    assert_eq!(op.len(), 1);
    assert_eq!(op[0].releases.count(), 2);
}

#[tokio::test]
async fn seeded_run_reuses_known_paths() {
    let dir = tempfile::tempdir().unwrap();
    write_fixture(dir.path());
    let eval = fake_eval(dir.path());
    let base = serve(dir.path()).await;
    let fetcher = Fetcher::new(1).unwrap();
    let a = ReleaseLocation::parse(&format!(
        "{base}/releases/nixos/25.05/nixos-25.05.100.aaaaaaaaaaaa"
    ))
    .unwrap();
    let bl = ReleaseLocation::parse(&format!(
        "{base}/releases/nixos/25.05/nixos-25.05.200.bbbbbbbbbbbb"
    ))
    .unwrap();
    let o = opts(&base, vec![Dataset::Closure], &eval);
    let seed = index_releases(&fetcher, std::slice::from_ref(&a), &o, None)
        .await
        .unwrap();

    // Remove narinfos that the seed already knows: a seeded run must not need them.
    for n in [1u32, 2, 3] {
        std::fs::remove_file(dir.path().join("cache").join(format!("{}.narinfo", hash(n)))).unwrap();
    }
    let b = index_releases(&fetcher, std::slice::from_ref(&bl), &o, Some(seed.clone()))
        .await
        .unwrap();
    b.validate().unwrap();
    assert_eq!(b.releases.len(), 1);
    assert_eq!(b.releases[0].name, bl.name);
    assert_eq!(
        names(b.closure.as_ref().unwrap(), 0),
        vec!["p1", "p2", "p3", "p5"]
    );
    assert!(b.closure.as_ref().unwrap().missing.is_empty());

    let mut o2 = o.clone();
    o2.include_seed = true;
    let both = index_releases(&fetcher, std::slice::from_ref(&bl), &o2, Some(seed))
        .await
        .unwrap();
    assert_eq!(both.releases.len(), 2);
}

fn script(dir: &Path, name: &str, body: &str) -> String {
    let p = dir.join(name);
    std::fs::write(&p, format!("#!/bin/sh\n{body}")).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p.display().to_string()
}

/// Helper closure types, with fake `nix-instantiate` and `nix`: probes
/// `fetchurl` (two calls sharing a curl-like input built from an uncached
/// source tarball), `stdenv` (a target) and `broken` (fails to evaluate).
#[tokio::test]
async fn helper_probe_closures() {
    let dir = tempfile::tempdir().unwrap();
    write_fixture(dir.path());
    let base = serve(dir.path()).await;
    let fetcher = Fetcher::new(1).unwrap();
    let drv = |n: u32, name: &str| format!("/nix/store/{}-{name}.drv", hash(n));
    let out = |n: u32, name: &str| format!("/nix/store/{}-{name}", hash(n));
    let instantiate = script(
        dir.path(),
        "fake-nix-instantiate",
        &format!(
            r#"case "$*" in
  *--eval*) echo '{{"broken":"fetch","fetchurl":"fetch","stdenv":"target"}}' ;;
  *"probes'.broken."*) echo "error: attribute 'nope' missing" >&2; exit 1 ;;
  *"probes'.fetchurl."*) echo {a}; echo {b} ;;
  *"probes'.stdenv."*) echo {s} ;;
  *) exit 2 ;;
esac
"#,
            a = drv(2001, "probe-a.tar.gz"),
            b = drv(2002, "probe-b.tar.gz"),
            s = drv(2003, "stdenv"),
        ),
    );
    let graph = serde_json::json!({"derivations": {
        drv(2001, "probe-a.tar.gz"): {"outputs": {"out": {"path": out(91, "probe-a.tar.gz")}},
            "inputs": {"drvs": {drv(2004, "curl"): {"outputs": ["out"]}, drv(2006, "arg-a"): {"outputs": ["out"]}}, "srcs": []}},
        drv(2002, "probe-b.tar.gz"): {"outputs": {"out": {"path": out(92, "probe-b.tar.gz")}},
            "inputs": {"drvs": {drv(2004, "curl"): {"outputs": ["out"]}}, "srcs": []}},
        drv(2004, "curl"): {"outputs": {"out": {"path": out(4, "p4")}},
            "inputs": {"drvs": {drv(2005, "curl.tar.xz"): {"outputs": ["out"]}}, "srcs": []}},
        drv(2005, "curl.tar.xz"): {"outputs": {"out": {"path": out(98, "curl.tar.xz")}}, "inputs": {"drvs": {}, "srcs": []}},
        drv(2006, "arg-a"): {"outputs": {"out": {"path": out(97, "arg-a")}}, "inputs": {"drvs": {}, "srcs": []}},
        drv(2003, "stdenv"): {"outputs": {"out": {"path": out(5, "p5")}}, "inputs": {"drvs": {}, "srcs": []}},
    }, "version": 4});
    std::fs::write(dir.path().join("graph.json"), graph.to_string()).unwrap();
    let nix = script(
        dir.path(),
        "fake-nix",
        &format!("cat {}\n", dir.path().join("graph.json").display()),
    );
    let mut o = opts(
        &base,
        vec![Dataset::Closure, Dataset::EvalErrors],
        Path::new("/bin/false"),
    );
    o.closure_types = vec![STORE_PATHS.into(), HELPERS_DIRECT.into(), HELPERS_FULL.into()];
    o.probes = ProbeOptions {
        systems: vec!["x86_64-linux".into()],
        nix_instantiate: vec![instantiate],
        nix: vec![nix],
        jobs: 2,
    };
    let a = ReleaseLocation::parse(&format!(
        "{base}/releases/nixos/25.05/nixos-25.05.100.aaaaaaaaaaaa"
    ))
    .unwrap();
    let b = index_releases(&fetcher, &[a], &o, None).await.unwrap();
    b.validate().unwrap();
    let c = b.closure.as_ref().unwrap();
    assert_eq!(c.closures.len(), 3);
    assert_eq!(names(c, 0), vec!["p1", "p2", "p3", "p4"]);
    // curl (p4) and stdenv (p5), with their runtime closures; not the
    // argument-specific derivation (97) or the fetches themselves.
    assert_eq!(
        typed_names(c, 0, HELPERS_DIRECT),
        vec!["p1", "p2", "p3", "p4", "p5"]
    );
    assert_eq!(
        typed_names(c, 0, HELPERS_FULL),
        vec!["p1", "p2", "p3", "p4", "p5"]
    );
    // The uncached source tarball is missing from helpers-full only.
    let full = c.closure_index(0, HELPERS_FULL).unwrap();
    let direct = c.closure_index(0, HELPERS_DIRECT).unwrap();
    let tarball = c.missing.iter().find(|m| m.name == "curl.tar.xz").unwrap();
    assert!(tarball.closures.get(full) && !tarball.closures.get(direct));
    // The probe that fails to evaluate is an eval error of the probes'
    // evaluation; the message is not kept.
    let e = b.eval_errors.as_ref().unwrap();
    assert_eq!(
        e.evals,
        vec![Eval {
            release: 0,
            eval_type: HELPER_PROBES.into(),
        }]
    );
    assert_eq!(
        e.errors,
        vec![EvalError {
            attr: "broken".into(),
            system: "x86_64-linux".into(),
            evals: Bitset::single(0),
        }]
    );
    assert!(b.releases[0].datasets.contains(&Dataset::EvalErrors));
}

/// Opt-in live smoke test (`NFI_LIVE_TESTS=1`): index a small sample of a real
/// release's closure plus its packages and programs.
#[tokio::test]
async fn live_smoke() {
    if std::env::var("NFI_LIVE_TESTS").as_deref() != Ok("1") {
        eprintln!("skipping live smoke test (set NFI_LIVE_TESTS=1)");
        return;
    }
    let url = std::env::var("NFI_LIVE_RELEASE_URL")
        .unwrap_or_else(|_| "https://releases.nixos.org/nixos/25.05/nixos-25.05.813814.ac62194c3917".into());
    let fetcher = Fetcher::new(4).unwrap();
    let loc = ReleaseLocation::parse(&url).unwrap();
    let opts = IndexOptions {
        datasets: vec![Dataset::Closure, Dataset::Packages, Dataset::Programs],
        closure_types: vec![STORE_PATHS.into()],
        probes: ProbeOptions::default(),
        closure: ClosureOptions {
            sample: Some(25),
            ..Default::default()
        },
        outpaths: OutpathsOptions::default(),
        include_seed: false,
    };
    let b = index_releases(&fetcher, &[loc], &opts, None).await.unwrap();
    b.validate().unwrap();
    assert_eq!(b.releases[0].commit.len(), 40);
    assert!(b.closure.as_ref().unwrap().paths.len() >= 25);
    assert!(b.packages.as_ref().unwrap().entries.len() > 50_000);
    assert!(b.programs.as_ref().unwrap().entries.len() > 10_000);
    let mut buf = Vec::new();
    nfi_interchange::codec::write(
        &b,
        &mut buf,
        nfi_interchange::Format::Binary(nfi_interchange::Compression::DEFAULT),
    )
    .unwrap();
    assert_eq!(nfi_interchange::codec::read(&buf[..], STORE_PATHS).unwrap(), b);
}

/// Runs the real nix-eval-jobs (when on PATH and able to evaluate) over a
/// miniature nixexprs tree.
#[tokio::test]
async fn real_nix_eval_jobs() {
    let ok = std::process::Command::new("nix-eval-jobs")
        .arg("--help")
        .output()
        .is_ok_and(|o| o.status.success());
    if !ok {
        eprintln!("skipping: nix-eval-jobs not available");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let top = dir.path().join("nixos-test");
    std::fs::create_dir_all(top.join("pkgs/top-level")).unwrap();
    std::fs::write(
        top.join("pkgs/top-level/packages-config.nix"),
        "{ allowAliases = false; }",
    )
    .unwrap();
    std::fs::write(
        top.join("default.nix"),
        r#"{ system, config }:
let mk = name: outputs: derivation { inherit name system outputs; builder = "/bin/sh"; args = [ "-c" "true" ]; };
in {
  hello = mk "hello-1.0" [ "out" ];
  multi = mk "multi-2.0" [ "out" "dev" ];
  nested = { recurseForDerivations = true; inner = mk "inner-3" [ "out" ]; };
  hidden = { inner = mk "hidden" [ "out" ]; };
  broken = throw "evaluation error";
}"#,
    )
    .unwrap();
    let opts = OutpathsOptions {
        workers: 1,
        max_memory_mb: 512,
        ..Default::default()
    };
    let res = crate::outpaths::evaluate(&top, &opts, 0).await;
    let entries = match res {
        Ok(e) => e,
        // e.g. no nix daemon/store access inside a build sandbox
        Err(e) => {
            eprintln!("skipping: nix-eval-jobs could not evaluate: {e:#}");
            return;
        }
    };
    let mut attrs: Vec<(&str, Vec<&str>)> = entries
        .iter()
        .map(|e| (e.attr.as_str(), e.outputs.keys().map(String::as_str).collect()))
        .collect();
    attrs.sort();
    assert_eq!(
        attrs,
        vec![
            ("hello", vec!["out"]),
            ("multi", vec!["dev", "out"]),
            ("nested.inner", vec!["out"])
        ]
    );
    assert!(
        entries
            .iter()
            .all(|e| e.drv_path.as_deref().is_some_and(|d| d.ends_with(".drv")))
    );
    assert!(entries.iter().all(|e| e.system == "x86_64-linux"));
}
