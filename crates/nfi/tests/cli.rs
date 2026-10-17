//! Query CLI against an in-process service.

use nfi_testenv::{A, h, path};
use serde_json::json;
use std::path::Path;
use std::process::Command;

struct Out {
    code: i32,
    stdout: String,
    stderr: String,
}

fn nfi(url: &str, dir: &Path, args: &[&str]) -> Out {
    let out = Command::new(env!("CARGO_BIN_EXE_nfi"))
        .args(["--url", url])
        .args(args)
        // Isolate from any user config.
        .env("XDG_CONFIG_HOME", dir)
        .env_remove("NFI_FETCH_HOOK")
        .output()
        .unwrap();
    Out {
        code: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

async fn ingest(svc: &nfi_testenv::TestService) {
    let mut data = Vec::new();
    nfi_interchange::codec::write(&nfi_testenv::bundle(), &mut data, nfi_interchange::Format::Json).unwrap();
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
    for _ in 0..300 {
        let j: serde_json::Value = http
            .get(format!("{}/admin/v1/jobs/{id}", svc.admin_url))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        match j["status"].as_str() {
            Some("succeeded") => return,
            Some("failed") => panic!("ingest failed: {j}"),
            _ => tokio::time::sleep(std::time::Duration::from_millis(100)).await,
        }
    }
    panic!("ingest timed out");
}

fn graph_json() -> serde_json::Value {
    let drv = |n: u8, name: &str| format!("/nix/store/{}-{name}.drv", h(100 + n));
    json!({
        drv(1, "app"): {"outputs": {"out": {"path": format!("/nix/store/{}-app", h(50))}},
            "inputDrvs": {drv(2, "hello"): ["out"], drv(3, "curl"): ["out"]}, "inputSrcs": []},
        drv(2, "hello"): {"outputs": {"out": {"path": path(2, "hello-2.12")}}, "inputDrvs": {}, "inputSrcs": []},
        drv(3, "curl"): {"outputs": {"out": {"path": path(3, "curl-8.0")}}, "inputDrvs": {}, "inputSrcs": []},
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn query_cli() {
    let Some(svc) = nfi_testenv::start_service(None).await else {
        return;
    };
    ingest(&svc).await;
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let url = svc.public_url.clone();
    let run = |args: &[&str]| tokio::task::block_in_place(|| nfi(&url, d, args));

    // show
    let o = run(&["show", "curl", "--commit", "aaaaaaaa"]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stdout.contains(&path(3, "curl-8.0")));
    assert!(o.stdout.contains("(not cached)"));
    let o = run(&["--json", "show", "hello", "--commit", A]);
    let v: serde_json::Value = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(v["outputs"]["out"]["cached"], true);
    let o = run(&["show", "nope", "--commit", A]);
    assert_eq!(o.code, 1);
    assert!(o.stderr.contains("not found"));

    // bulk without hook prints the closure
    let o = run(&["bulk", &format!("hello@{A}"), "missing@aaaa"]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    let lines: Vec<&str> = o.stdout.lines().collect();
    let mut expect = vec![path(1, "glibc-2.40"), path(2, "hello-2.12")];
    expect.sort();
    assert_eq!(lines, expect);
    assert!(o.stderr.contains("UnknownAttr"));
    let o = run(&["bulk", "--strict", &format!("hello@{A}"), "missing@aaaa"]);
    assert_eq!(o.code, 2);

    // bulk from a file with a fetch hook using {} substitution; hook output reaches the terminal.
    let items = d.join("items.txt");
    std::fs::write(&items, format!("# comment\ncurl {A}\n\nhello@{A}@x86_64-linux\n")).unwrap();
    let copy = d.join("copied.txt");
    let hook = format!("sh -c 'cp \"$1\" {} && echo hook-ran' _ {{}}", copy.display());
    let o = run(&["bulk", "-f", items.to_str().unwrap(), "--fetch-hook", &hook]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(o.stdout.trim(), "hook-ran");
    let copied = std::fs::read_to_string(&copy).unwrap();
    assert_eq!(copied.lines().count(), 4);
    assert!(copied.contains(&path(4, "openssl-3.0")));

    // Hook exit codes are propagated; the file path is appended when there is no {}.
    let o = run(&[
        "bulk",
        &format!("hello@{A}"),
        "--fetch-hook",
        "sh -c 'test -s \"$0\" && exit 7'",
    ]);
    assert_eq!(o.code, 7);

    // Hook from the config file (and NFI_FETCH_HOOK override is absent).
    std::fs::create_dir_all(d.join("nfi")).unwrap();
    let cfg_copy = d.join("cfg-copied.txt");
    std::fs::write(
        d.join("nfi/config.toml"),
        format!("fetch_hook = \"cp {{}} {}\"\n", cfg_copy.display()),
    )
    .unwrap();
    let o = run(&["bulk", &format!("hello@{A}")]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(std::fs::read_to_string(&cfg_copy).unwrap().lines().count(), 2);
    let o = run(&["bulk", &format!("hello@{A}"), "--no-fetch-hook"]);
    assert_eq!(o.stdout.lines().count(), 2);
    std::fs::remove_file(d.join("nfi/config.toml")).unwrap();

    // minimise from a JSON file and stdin-free paths.
    let gfile = d.join("graph.json");
    std::fs::write(&gfile, graph_json().to_string()).unwrap();
    let o = run(&["--json", "minimise", "-f", gfile.to_str().unwrap()]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    let m: serde_json::Value = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(m["to_build"].as_array().unwrap().len(), 1);
    assert_eq!(m["roots"], json!([path(2, "hello-2.12"), path(3, "curl-8.0")]));
    assert_eq!(m["closure"].as_array().unwrap().len(), 4);
    let roots_file = d.join("roots.txt");
    let o = run(&[
        "minimise",
        "-f",
        gfile.to_str().unwrap(),
        "--roots-only",
        "--fetch-hook",
        &format!("cp {{}} {}", roots_file.display()),
    ]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(std::fs::read_to_string(&roots_file).unwrap().lines().count(), 2);

    // An uncached stdenv in the graph produces an unindexed-nixpkgs warning on stderr.
    let stdenv_file = d.join("stdenv-graph.json");
    let sdrv = format!("/nix/store/{}-stdenv-linux.drv", h(150));
    std::fs::write(
        &stdenv_file,
        json!({ sdrv.clone(): {"outputs": {"out": {"path": format!("/nix/store/{}-stdenv-linux", h(151))}}, "inputDrvs": {}, "inputSrcs": []} }).to_string(),
    )
    .unwrap();
    let o = run(&["minimise", "-f", stdenv_file.to_str().unwrap()]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(
        o.stderr.contains("warning: the standard environment"),
        "{}",
        o.stderr
    );
    assert!(o.stderr.contains("not indexed"), "{}", o.stderr);
    let o = run(&["minimise", "-f", gfile.to_str().unwrap()]);
    assert!(!o.stderr.contains("standard environment"), "{}", o.stderr);

    // minimise with installables resolved through a (fake) nix command.
    let fake = d.join("fake-nix");
    let args_log = d.join("nix-args");
    std::fs::write(
        &fake,
        format!(
            "#!/bin/sh\necho \"$@\" > {}\ncat {}\n",
            args_log.display(),
            gfile.display()
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let o = run(&[
        "minimise",
        "--nix",
        fake.to_str().unwrap(),
        ".#app",
        "nixpkgs#hello",
    ]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(o.stdout.lines().count(), 4);
    let logged = std::fs::read_to_string(&args_log).unwrap();
    assert!(
        logged.contains("derivation show --recursive .#app nixpkgs#hello"),
        "{logged}"
    );

    // Real nix, when available: a local derivation is requested and must be built.
    if Command::new("nix-instantiate")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
    {
        let expr = "derivation { name = \"nfi-cli-test\"; system = builtins.currentSystem; builder = \"/bin/sh\"; args = [\"-c\" \"echo > $out\"]; }";
        let inst = Command::new("nix-instantiate")
            .args(["--expr", expr])
            .output()
            .unwrap();
        if inst.status.success() {
            let drv = String::from_utf8_lossy(&inst.stdout).trim().to_string();
            let o = run(&["--json", "minimise", &format!("{drv}^*")]);
            assert_eq!(o.code, 0, "{}", o.stderr);
            let m: serde_json::Value = serde_json::from_str(&o.stdout).unwrap();
            assert_eq!(m["requested"], json!([drv]));
            assert_eq!(m["to_build"], json!([drv]));
        }
    }

    if svc.has_search {
        let o = run(&["search", "greeting"]);
        assert_eq!(o.code, 0, "{}", o.stderr);
        assert!(o.stdout.starts_with("hello 2.12"), "{}", o.stdout);
        let o = run(&["--json", "search", "--program", "curl"]);
        let v: serde_json::Value = serde_json::from_str(&o.stdout).unwrap();
        assert_eq!(v["hits"][0]["attr"], "curl");
    }

    // Errors from the service are reported.
    let o = run(&["minimise", "-f", items.to_str().unwrap()]);
    assert_eq!(o.code, 1);
}
