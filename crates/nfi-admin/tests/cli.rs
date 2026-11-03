//! Admin CLI against an in-process service.

use std::process::Command;

fn admin(svc: &nfi_testenv::TestService, args: &[&str]) -> (i32, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_nfi-admin"))
        .args(["--url", &svc.admin_url])
        .env("NFI_ADMIN_TOKEN", "tok")
        .env_remove("NFI_ADMIN_TOKEN_FILE")
        .args(args)
        .output()
        .unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn admin_cli_round_trip() {
    let Some(svc) = nfi_testenv::start_service(Some("tok")).await else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("b.nfib");
    nfi_interchange::codec::write_file(
        &nfi_testenv::bundle(),
        &file,
        nfi_interchange::Format::Binary(nfi_interchange::Compression::DEFAULT),
    )
    .unwrap();

    let svc2 = &svc;
    let (code, out, err) =
        tokio::task::block_in_place(|| admin(svc2, &["ingest", "--wait", file.to_str().unwrap()]));
    assert_eq!(code, 0, "{out}\n{err}");
    assert!(out.contains("succeeded"), "{out}");

    let (code, out, _) = tokio::task::block_in_place(|| admin(svc2, &["releases"]));
    assert_eq!(code, 0);
    assert!(out.contains("nixos-25.05.100.aaaaaaaaaaaa"));
    assert!(out.contains("closure,outpaths,packages,programs"));

    let (_, out, _) =
        tokio::task::block_in_place(|| admin(svc2, &["--json", "releases", "--channel", "nixos-unstable"]));
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v.as_array().unwrap().len(), 1);

    let (code, out, _) =
        tokio::task::block_in_place(|| admin(svc2, &["prune", "--channel", "nixos-unstable", "--wait"]));
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("nixos-26.11pre200.bbbbbbbbbbbb"));

    // Consistency check: clean index exits 0.
    let (code, out, err) = tokio::task::block_in_place(|| admin(svc2, &["check", "--wait"]));
    assert_eq!(code, 0, "{out}\n{err}");
    assert!(out.starts_with("ok:"), "{out}");

    let (code, out, _) = tokio::task::block_in_place(|| admin(svc2, &["jobs"]));
    assert_eq!(code, 0);
    assert_eq!(out.matches("succeeded").count(), 3);

    // An inconsistency makes `check --wait` exit 3 and name the problem.
    sqlx::query("UPDATE store_paths SET refs = refs || 999999::bigint WHERE name = 'glibc-2.40'")
        .execute(&svc.state.pool)
        .await
        .unwrap();
    let (code, out, _) = tokio::task::block_in_place(|| admin(svc2, &["check", "--wait"]));
    assert_eq!(code, 3, "{out}");
    assert!(
        out.contains("INCONSISTENT") && out.contains("dangling_refs (1)"),
        "{out}"
    );
    let (code, out, _) = tokio::task::block_in_place(|| admin(svc2, &["--json", "check", "--wait"]));
    assert_eq!(code, 3);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["ok"], false);
    sqlx::query("UPDATE store_paths SET refs = array_remove(refs, 999999::bigint)")
        .execute(&svc.state.pool)
        .await
        .unwrap();

    // Failing ingest exits with status 2; bad token fails with 1.
    let bad = dir.path().join("bad");
    std::fs::write(&bad, "nope").unwrap();
    let (code, _, _) =
        tokio::task::block_in_place(|| admin(svc2, &["ingest", "--wait", bad.to_str().unwrap()]));
    assert_eq!(code, 2);
    let out = Command::new(env!("CARGO_BIN_EXE_nfi-admin"))
        .args(["--url", &svc.admin_url, "releases"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("401"));
    let (code, _, err) = tokio::task::block_in_place(|| admin(svc2, &["prune"]));
    assert_eq!(code, 1, "{err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_admin_token_hint() {
    let Some(svc) = nfi_testenv::start_service(Some("tok")).await else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let run = |token: Option<&str>, args: &[&str]| {
        let mut c = Command::new(env!("CARGO_BIN_EXE_nfi-admin"));
        c.args(["--url", &svc.admin_url])
            .args(args)
            .env("NFI_TOKEN_CACHE", dir.path().join("tokens"))
            .env_remove("NFI_ADMIN_TOKEN")
            .env_remove("NFI_ADMIN_TOKEN_FILE");
        if let Some(t) = token {
            c.env("NFI_ADMIN_TOKEN", t);
        }
        let out = tokio::task::block_in_place(|| c.output().unwrap());
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    };
    let (code, _, err) = run(None, &["releases"]);
    assert_eq!(code, Some(1), "{err}");
    // A fixed-token listener has no OIDC login to suggest.
    assert!(err.contains("set NFI_ADMIN_TOKEN or pass --token-file"), "{err}");
    assert!(!err.contains("nfi-admin login"), "{err}");

    // `whoami` checks the static token.
    let url = &svc.admin_url;
    for (token, code, out) in [
        (
            Some("tok"),
            0,
            format!("{url}: authenticated with a static token"),
        ),
        (Some("wrong"), 1, format!("{url}: not authenticated")),
        (None, 1, format!("{url}: not authenticated")),
    ] {
        let (c, o, e) = run(token, &["whoami"]);
        assert_eq!((c, o.trim()), (Some(code), out.as_str()), "{e}");
    }

    // The token is never taken from the command line.
    let (code, _, err) = run(None, &["--token", "tok", "releases"]);
    assert_eq!(code, Some(2), "{err}");
    assert!(err.contains("unexpected argument '--token'"), "{err}");
}
