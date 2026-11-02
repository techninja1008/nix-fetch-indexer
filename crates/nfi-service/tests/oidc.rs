//! OIDC authentication and CEL authorisation of both listeners.

use nfi_auth::{AuthConfig, CONFIG_PATH, WHOAMI_PATH, Whoami};
use nfi_auth_server::{Authenticator, Expr, OidcSettings};
use nfi_service::auth::{AdminAuth, ServiceAuth};
use nfi_test_support::MockIdp;
use serde_json::json;
use std::sync::Arc;

fn auth(idp: &MockIdp, allow_anonymous: bool, admin: AdminAuth) -> ServiceAuth {
    let mut s = OidcSettings::new(idp.issuer(), idp.client_id());
    s.session = Some(Expr::new("has(claims.sid) ? claims.sid : null").unwrap());
    ServiceAuth::oidc(
        Arc::new(Authenticator::new(s).unwrap()),
        Expr::new("has(claims.groups) && 'nfi' in claims.groups && request.method in ['GET', 'POST']")
            .unwrap(),
        allow_anonymous,
        admin,
    )
}

async fn get(url: String, token: Option<&str>) -> reqwest::Response {
    let r = reqwest::Client::new().get(url);
    match token {
        Some(t) => r.bearer_auth(t),
        None => r,
    }
    .send()
    .await
    .unwrap()
}

#[tokio::test]
async fn public_api() {
    let idp = MockIdp::start("nfi").await;
    let Some(svc) = nfi_testenv::start_service_with(auth(&idp, false, AdminAuth::Open)).await else {
        return;
    };
    let u = |p: &str| format!("{}{p}", svc.public_url);
    let good = idp.token(json!({"sub": "1", "preferred_username": "alice", "groups": ["nfi"], "sid": "s1"}));
    let outsider = idp.token(json!({"sub": "2", "groups": ["other"]}));
    let foreign = idp.token(json!({"sub": "1", "groups": ["nfi"], "aud": "another-client"}));

    // Exempt routes.
    assert_eq!(get(u("/healthz"), None).await.status(), 200);
    let cfg: AuthConfig = get(u(CONFIG_PATH), None).await.json().await.unwrap();
    assert_eq!(cfg.issuer, idp.issuer());
    assert_eq!(cfg.client_id, "nfi");
    assert!(cfg.scopes.contains(&"openid".to_string()));
    assert!(!cfg.anonymous);

    let r = get(u("/v1/releases"), None).await;
    assert_eq!(r.status(), 401);
    assert_eq!(r.headers()["www-authenticate"], "Bearer");
    assert!(r.json::<serde_json::Value>().await.unwrap()["error"].is_string());
    assert_eq!(get(u("/v1/releases"), Some("garbage")).await.status(), 401);
    assert_eq!(get(u("/v1/releases"), Some(&foreign)).await.status(), 401);
    assert_eq!(get(u("/v1/releases"), Some(&outsider)).await.status(), 403);
    assert_eq!(get(u("/v1/releases"), Some(&good)).await.status(), 200);

    let me: Whoami = get(u(WHOAMI_PATH), Some(&good)).await.json().await.unwrap();
    assert_eq!(me.username, "alice");
    assert_eq!(me.session.as_deref(), Some("s1"));
    // By default, additional_info is the token's claims.
    assert_eq!(me.additional_info["groups"], json!(["nfi"]));
    assert_eq!(me.additional_info["iss"], idp.issuer());
    assert_eq!(get(u(WHOAMI_PATH), None).await.status(), 401);
    // Whoami does not go through the policy.
    let me: Whoami = get(u(WHOAMI_PATH), Some(&outsider)).await.json().await.unwrap();
    assert_eq!(me.username, "2");

    // The admin listener is open, and advertises no OIDC.
    assert_eq!(
        get(format!("{}/admin/v1/releases", svc.admin_url), None)
            .await
            .status(),
        200
    );
    assert_eq!(
        get(format!("{}{CONFIG_PATH}", svc.admin_url), None)
            .await
            .status(),
        404
    );
    assert_eq!(
        get(format!("{}{WHOAMI_PATH}", svc.admin_url), None)
            .await
            .status(),
        404
    );

    // The service binary has the auth flags (and clap accepts their relations).
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_nfi-service"))
        .arg("--help")
        .output()
        .unwrap();
    let help = String::from_utf8_lossy(&out.stdout);
    for flag in [
        "--oidc-issuer",
        "--admin-token-file",
        "--allow-anonymous",
        "--admin-oidc",
        "--main-authz-cel-file",
        "--oidc-whoami-cel-file",
    ] {
        assert!(help.contains(flag), "{flag} missing from --help");
    }
}

#[tokio::test]
async fn anonymous_allowed() {
    let idp = MockIdp::start("nfi").await;
    let Some(svc) = nfi_testenv::start_service_with(auth(&idp, true, AdminAuth::Open)).await else {
        return;
    };
    let u = |p: &str| format!("{}{p}", svc.public_url);
    let cfg: AuthConfig = get(u(CONFIG_PATH), None).await.json().await.unwrap();
    assert!(cfg.anonymous);
    // Anonymous requests skip the policy; tokens are still checked.
    assert_eq!(get(u("/v1/releases"), None).await.status(), 200);
    assert_eq!(get(u("/v1/releases"), Some("garbage")).await.status(), 401);
    let outsider = idp.token(json!({"sub": "2"}));
    assert_eq!(get(u("/v1/releases"), Some(&outsider)).await.status(), 403);
    assert_eq!(get(u(WHOAMI_PATH), None).await.status(), 401);
}

#[tokio::test]
async fn admin_oidc() {
    let idp = MockIdp::start("nfi").await;
    let policy = Expr::new("user.username == 'root' && !request.path.startsWith('/admin/v1/prune')").unwrap();
    let Some(svc) = nfi_testenv::start_service_with(auth(&idp, false, AdminAuth::Oidc(policy))).await else {
        return;
    };
    let u = |p: &str| format!("{}{p}", svc.admin_url);
    let root = idp.token(json!({"sub": "0", "preferred_username": "root"}));
    let alice = idp.token(json!({"sub": "1", "preferred_username": "alice", "groups": ["nfi"]}));
    assert_eq!(get(u("/healthz"), None).await.status(), 200);
    let cfg: AuthConfig = get(u(CONFIG_PATH), None).await.json().await.unwrap();
    assert_eq!(cfg.client_id, "nfi");
    assert!(!cfg.anonymous);
    assert_eq!(get(u("/admin/v1/jobs"), None).await.status(), 401);
    assert_eq!(get(u("/admin/v1/jobs"), Some(&alice)).await.status(), 403);
    assert_eq!(get(u("/admin/v1/jobs"), Some(&root)).await.status(), 200);
    let me: Whoami = get(u(WHOAMI_PATH), Some(&root)).await.json().await.unwrap();
    assert_eq!(me.username, "root");
    let r = reqwest::Client::new()
        .post(u("/admin/v1/prune"))
        .bearer_auth(&root)
        .json(&json!({"releases": ["x"]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    // The main policy applies to the public listener, the admin one to the admin listener.
    assert_eq!(
        get(format!("{}/v1/releases", svc.public_url), Some(&root))
            .await
            .status(),
        403
    );
    assert_eq!(
        get(format!("{}/v1/releases", svc.public_url), Some(&alice))
            .await
            .status(),
        200
    );
}

#[tokio::test]
async fn admin_token_with_oidc_public() {
    let idp = MockIdp::start("nfi").await;
    let Some(svc) =
        nfi_testenv::start_service_with(auth(&idp, false, AdminAuth::Token("s3cret".into()))).await
    else {
        return;
    };
    let u = |p: &str| format!("{}{p}", svc.admin_url);
    let alice = idp.token(json!({"sub": "1", "groups": ["nfi"]}));
    assert_eq!(get(u("/admin/v1/jobs"), Some(&alice)).await.status(), 401);
    assert_eq!(get(u("/admin/v1/jobs"), Some("s3cret")).await.status(), 200);
    assert_eq!(get(u(CONFIG_PATH), None).await.status(), 404);
    assert_eq!(get(u(WHOAMI_PATH), Some("s3cret")).await.status(), 404);
}

#[test]
fn config_errors() {
    let d = tempfile::tempdir().unwrap();
    let tok = d.path().join("token");
    std::fs::write(&tok, "t\n").unwrap();
    let cel = d.path().join("admin.cel");
    std::fs::write(&cel, "true").unwrap();
    let bad = d.path().join("bad.cel");
    std::fs::write(&bad, "(((").unwrap();
    let run = |args: &[&str]| {
        let mut c = std::process::Command::new(env!("CARGO_BIN_EXE_nfi-service"));
        c.args(["--database-url", "postgres://127.0.0.1:1/x"]).args(args);
        // Keep the rest of the environment: HTTP clients need SSL_CERT_FILE
        // where there are no system certificates (the Nix sandbox).
        for (k, _) in std::env::vars_os() {
            if k.to_string_lossy().starts_with("NFI_") {
                c.env_remove(k);
            }
        }
        let out = c.output().unwrap();
        assert!(!out.status.success(), "{args:?} should fail");
        String::from_utf8_lossy(&out.stderr).into_owned()
    };
    let tok = tok.to_str().unwrap();
    let cel = cel.to_str().unwrap();
    let oidc = ["--oidc-issuer", "http://idp", "--oidc-client-id", "nfi"];
    let e = run(&[
        &oidc[..],
        &[
            "--admin-oidc",
            "--admin-authz-cel-file",
            cel,
            "--admin-token-file",
            tok,
        ],
    ]
    .concat());
    assert!(e.contains("cannot be used with"), "{e}");
    let e = run(&["--admin-oidc", "--admin-authz-cel-file", cel]);
    assert!(e.contains("--oidc-issuer"), "{e}");
    let e = run(&[&oidc[..], &["--admin-oidc"]].concat());
    assert!(e.contains("--admin-authz-cel-file"), "{e}");
    let e = run(&["--allow-anonymous"]);
    assert!(e.contains("--oidc-issuer"), "{e}");
    let e = run(&["--oidc-issuer", "http://idp"]);
    assert!(e.contains("--oidc-client-id"), "{e}");
    let e = run(&[&oidc[..], &["--main-authz-cel-file", bad.to_str().unwrap()]].concat());
    assert!(e.contains("compiling CEL"), "{e}");
    let e = run(&[&oidc[..], &["--oidc-whoami-cel-file", bad.to_str().unwrap()]].concat());
    assert!(e.contains("compiling CEL"), "{e}");
}

/// A provider whose discovery document names another issuer stops the
/// service at startup (an unreachable one does not: see `config_errors`,
/// which gets as far as the database).
#[tokio::test(flavor = "multi_thread")]
async fn issuer_mismatch() {
    let idp = MockIdp::start("nfi").await;
    let issuer = idp.issuer().replace("127.0.0.1", "localhost");
    let mut c = tokio::process::Command::new(env!("CARGO_BIN_EXE_nfi-service"));
    c.args(["--database-url", "postgres://127.0.0.1:1/x"]).args([
        "--oidc-issuer",
        &issuer,
        "--oidc-client-id",
        "nfi",
    ]);
    for (k, _) in std::env::vars_os() {
        if k.to_string_lossy().starts_with("NFI_") {
            c.env_remove(k);
        }
    }
    let out = c.output().await.unwrap();
    assert!(!out.status.success());
    let e = String::from_utf8_lossy(&out.stderr);
    assert!(e.contains("issuer mismatch"), "{e}");
}
