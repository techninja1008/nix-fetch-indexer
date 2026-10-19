//! Opt-in live end-to-end test (`NFI_LIVE_TESTS=1`).
//!
//! Indexes a real release (or uses `NFI_LIVE_BUNDLE`, e.g. a full closure
//! merged with packages/programs), ingests it into an in-process service, and
//! runs real queries through the CLI, including `minimise` on derivations
//! evaluated by nix from the release's own nixexprs.

use std::process::Command;

const DEFAULT_RELEASE: &str = "https://releases.nixos.org/nixos/25.05/nixos-25.05.813814.ac62194c3917";

#[tokio::test(flavor = "multi_thread")]
async fn live_end_to_end() {
    if std::env::var("NFI_LIVE_TESTS").as_deref() != Ok("1") {
        eprintln!("skipping live end-to-end test (set NFI_LIVE_TESTS=1)");
        return;
    }
    let Some(svc) = nfi_testenv::start_service(None).await else {
        return;
    };
    let release_url = std::env::var("NFI_LIVE_RELEASE_URL").unwrap_or_else(|_| DEFAULT_RELEASE.into());
    let dir = tempfile::tempdir().unwrap();
    let bundle_path = match std::env::var("NFI_LIVE_BUNDLE") {
        Ok(p) => std::path::PathBuf::from(p),
        Err(_) => {
            let fetcher = nfi_indexer::fetch::Fetcher::new(4).unwrap();
            let loc = nfi_indexer::release::ReleaseLocation::parse(&release_url).unwrap();
            let opts = nfi_indexer::index::IndexOptions {
                datasets: vec![
                    nfi_interchange::Dataset::Closure,
                    nfi_interchange::Dataset::Packages,
                    nfi_interchange::Dataset::Programs,
                ],
                closure_types: vec![nfi_interchange::STORE_PATHS.into()],
                closure: nfi_indexer::closure::ClosureOptions {
                    sample: Some(200),
                    ..Default::default()
                },
                probes: Default::default(),
                outpaths: Default::default(),
                include_seed: false,
            };
            let b = nfi_indexer::index::index_releases(&fetcher, &[loc], &opts, None)
                .await
                .unwrap();
            let p = dir.path().join("live.nfib");
            nfi_interchange::codec::write_file(
                &b,
                &p,
                nfi_interchange::Format::Binary(nfi_interchange::Compression::Zstd(3)),
            )
            .unwrap();
            p
        }
    };
    let bundle = nfi_interchange::codec::read_file(&bundle_path, nfi_interchange::STORE_PATHS).unwrap();
    let commit = bundle.releases[0].commit.clone();
    let started = std::time::Instant::now();
    let data = std::fs::read(&bundle_path).unwrap();
    let http = reqwest::Client::new();
    let r: serde_json::Value = http
        .post(format!("{}/admin/v1/ingest", svc.admin_url))
        .body(data)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = r["id"].as_i64().unwrap();
    loop {
        let j: serde_json::Value = http
            .get(format!("{}/admin/v1/jobs/{id}", svc.admin_url))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        match j["status"].as_str() {
            Some("succeeded") => {
                eprintln!("ingested in {:?}: {}", started.elapsed(), j["result"]);
                break;
            }
            Some("failed") => panic!("ingest failed: {j}"),
            _ => tokio::time::sleep(std::time::Duration::from_millis(500)).await,
        }
    }
    let nfi = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_nfi"))
            .args(["--url", &svc.public_url])
            .args(args)
            .env("XDG_CONFIG_HOME", dir.path())
            .output()
            .unwrap();
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    };
    if svc.has_search {
        let (code, out, err) =
            tokio::task::block_in_place(|| nfi(&["search", "--program", "rg", "--limit", "5"]));
        assert_eq!(code, Some(0), "{err}");
        assert!(out.contains("ripgrep"), "{out}");
        let (_, out, _) = tokio::task::block_in_place(|| {
            nfi(&["--json", "search", "friendly greeting", "--commit", &commit])
        });
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert!(
            v["hits"].as_array().unwrap().iter().any(|h| h["attr"] == "hello"),
            "{out}"
        );
    }
    let (code, out, err) =
        tokio::task::block_in_place(|| nfi(&["--json", "show", "hello", "--commit", &commit[..12]]));
    assert_eq!(code, Some(0), "{err}");
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["pname"], "hello");

    // Real minimise, when nix can evaluate the release's nixexprs.
    let has_nix = Command::new("nix")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    let full_closure = std::env::var("NFI_LIVE_BUNDLE").is_ok();
    if has_nix && full_closure {
        let tarball = format!("{release_url}/nixexprs.tar.xz");
        let t = std::time::Instant::now();
        let (code, out, err) =
            tokio::task::block_in_place(|| nfi(&["--json", "minimise", &format!("{tarball}#hello")]));
        assert_eq!(code, Some(0), "{err}");
        let m: serde_json::Value = serde_json::from_str(&out).unwrap();
        eprintln!("minimise hello: {:?}: {err}", t.elapsed());
        assert_eq!(
            m["to_build"],
            serde_json::json!([]),
            "hello is cached in its own release"
        );
        assert_eq!(m["roots"].as_array().unwrap().len(), 1);
        assert!(m["closure"].as_array().unwrap().len() > 1);

        // A modified hello must be built; its cached build inputs are returned.
        let expr = format!(
            "(import (builtins.fetchTarball \"{tarball}\") {{}}).hello.overrideAttrs (o: {{ pname = \"hello-nfi-test\"; }})"
        );
        let show = Command::new("nix")
            .args([
                "--extra-experimental-features",
                "nix-command",
                "derivation",
                "show",
                "--recursive",
                "--impure",
                "--expr",
                &expr,
            ])
            .output()
            .unwrap();
        assert!(show.status.success(), "{}", String::from_utf8_lossy(&show.stderr));
        let gfile = dir.path().join("graph.json");
        std::fs::write(&gfile, &show.stdout).unwrap();
        let t = std::time::Instant::now();
        let (code, out, err) =
            tokio::task::block_in_place(|| nfi(&["--json", "minimise", "-f", gfile.to_str().unwrap()]));
        assert_eq!(code, Some(0), "{err}");
        let m: serde_json::Value = serde_json::from_str(&out).unwrap();
        eprintln!("minimise modified hello: {:?}: {err}", t.elapsed());
        // Fixed-output sources are not part of store-paths.xz closures, so the
        // source tarball shows up as a derivation to build alongside hello itself.
        let to_build: Vec<&str> = m["to_build"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r.as_str().unwrap())
            .collect();
        assert!(to_build[0].contains("hello-nfi-test"), "{to_build:?}");
        assert!(
            to_build[1..]
                .iter()
                .all(|d| d.contains(".tar.") || d.contains("-source")),
            "{to_build:?}"
        );
        let roots: Vec<&str> = m["roots"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r.as_str().unwrap())
            .collect();
        assert!(roots.iter().any(|r| r.contains("stdenv")), "{roots:?}");
    }
}
