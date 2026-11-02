//! The admin CLI against an OIDC-protected admin listener.

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use nfi_auth_client::{StoredToken, TokenStore};
use nfi_auth_server::{Authenticator, Expr, OidcSettings};
use nfi_service::auth::{AdminAuth, ServiceAuth};
use nfi_test_support::{Impostor, MockIdp};
use serde_json::json;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

struct Out {
    code: i32,
    stdout: String,
    stderr: String,
}

fn run(bin: &str, dir: &Path, env: &[(&str, &str)], args: &[&str]) -> Out {
    let out = Command::new(bin)
        .args(args)
        .env("XDG_CONFIG_HOME", dir)
        .env("NFI_TOKEN_CACHE", dir.join("tokens"))
        .env_remove("NFI_TOKEN")
        .env_remove("NFI_TOKEN_FILE")
        .env_remove("NFI_ADMIN_TOKEN")
        .env_remove("NFI_ADMIN_TOKEN_FILE")
        .env_remove("NFI_ALLOW_INSECURE")
        .envs(env.iter().copied())
        .stdin(Stdio::null())
        .output()
        .unwrap();
    Out {
        code: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn tokens() {
    let idp = MockIdp::start("nfi").await;
    let auth = ServiceAuth::oidc(
        std::sync::Arc::new(Authenticator::new(OidcSettings::new(idp.issuer(), idp.client_id())).unwrap()),
        Expr::new("true").unwrap(),
        false,
        AdminAuth::Oidc(Expr::new("user.username == 'root'").unwrap()),
    );
    let Some(svc) = nfi_testenv::start_service_with(auth).await else {
        return;
    };
    let d = tempfile::tempdir().unwrap();
    // A login made with `nfi login` for the public listener: the admin CLI
    // does not send it to the admin listener until logged in there.
    let store = TokenStore::at(d.path().join("tokens"));
    store
        .save_token(&StoredToken {
            issuer: idp.issuer().into(),
            client_id: idp.client_id().into(),
            id_token: idp.token(json!({"sub": "2", "preferred_username": "bob"})),
            refresh_token: None,
        })
        .unwrap();
    store.record_url(&svc.public_url, &idp.auth_config()).unwrap();
    let admin = |env: &[(&str, &str)], args: &[&str]| {
        let mut a = vec!["--url", svc.admin_url.as_str()];
        a.extend_from_slice(args);
        run(env!("CARGO_BIN_EXE_nfi-admin"), d.path(), env, &a)
    };
    // Without a terminal, the command stops before sending its request.
    let o = admin(&[], &["jobs"]);
    assert_eq!(o.code, 1);
    assert!(o.stderr.contains("run `nfi-admin login`"), "{}", o.stderr);
    assert!(!o.stderr.contains("401"), "{}", o.stderr);
    let o = admin(&[], &["whoami"]);
    assert_eq!(o.code, 1);
    assert_eq!(o.stdout.trim(), format!("{}: not authenticated", svc.admin_url));
    // `login` there reuses it without a terminal, although bob fails the
    // admin policy (whoami does not apply it); commands then get 403.
    let o = admin(&[], &["login"]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(
        o.stderr
            .contains(&format!("logged in to {} as bob", svc.admin_url)),
        "{}",
        o.stderr
    );
    let o = admin(&[], &["whoami"]);
    assert!(
        o.stdout.starts_with(&format!("{}: bob\n", svc.admin_url)),
        "{}",
        o.stdout
    );
    let o = admin(&[], &["jobs"]);
    assert_eq!(o.code, 1);
    assert!(o.stderr.contains("403"), "{}", o.stderr);
    assert_eq!(idp.device_logins(), 0);
    // A service at another URL claiming the same provider gets nothing.
    let evil = Impostor::start(&idp).await;
    let o = run(
        env!("CARGO_BIN_EXE_nfi-admin"),
        d.path(),
        &[],
        &["--url", evil.url(), "jobs"],
    );
    assert_eq!(o.code, 1);
    assert!(o.stderr.contains("run `nfi-admin login`"), "{}", o.stderr);
    assert!(evil.bearers().is_empty());
    assert_eq!((idp.config_fetches(), evil.requests()), (1, 0));
    let root = idp.token(json!({"sub": "0", "preferred_username": "root"}));
    let o = admin(&[("NFI_ADMIN_TOKEN", &root)], &["jobs"]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    let o = admin(&[("NFI_ADMIN_TOKEN", &root)], &["whoami"]);
    assert!(
        o.stdout.starts_with(&format!("{}: root\n", svc.admin_url)),
        "{}",
        o.stdout
    );
    // --token takes precedence over the environment.
    let o = admin(&[("NFI_ADMIN_TOKEN", "garbage")], &["--token", &root, "jobs"]);
    assert_eq!(o.code, 0, "{}", o.stderr);
}

/// A fake admin listener: ingesting URLs gives job ids; once a token has
/// been used to ingest, it is rejected (as if it had been revoked).
#[derive(Clone, Default)]
struct Fake {
    idp: Option<MockIdp>,
    ingested: Arc<Mutex<Vec<String>>>,
    revoked: Arc<Mutex<Vec<String>>>,
}

async fn fake_config(State(f): State<Fake>) -> Response {
    let idp = f.idp.unwrap();
    axum::Json(json!({"issuer": idp.issuer(), "client_id": idp.client_id(), "scopes": ["openid"]}))
        .into_response()
}

async fn fake_ingest(
    State(f): State<Fake>,
    h: HeaderMap,
    axum::Json(r): axum::Json<serde_json::Value>,
) -> Response {
    let token = h
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or_default()
        .to_string();
    if token.is_empty() || f.revoked.lock().unwrap().contains(&token) {
        return (
            StatusCode::UNAUTHORIZED,
            axum::Json(json!({"error": "invalid token"})),
        )
            .into_response();
    }
    f.revoked.lock().unwrap().push(token);
    let mut i = f.ingested.lock().unwrap();
    i.push(r["url"].as_str().unwrap().to_string());
    axum::Json(json!({"id": i.len()})).into_response()
}

#[tokio::test(flavor = "multi_thread")]
async fn rejected_login_mid_command() {
    let idp = MockIdp::start("nfi").await;
    let fake = Fake {
        idp: Some(idp.clone()),
        ..Default::default()
    };
    let app = Router::new()
        .route(nfi_auth::CONFIG_PATH, get(fake_config))
        .route("/admin/v1/ingest", post(fake_ingest))
        .with_state(fake.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });

    let d = tempfile::tempdir().unwrap();
    let store = TokenStore::at(d.path().join("tokens"));
    store
        .save_token(&StoredToken {
            issuer: idp.issuer().into(),
            client_id: idp.client_id().into(),
            id_token: idp.token(json!({"sub": "2", "preferred_username": "bob"})),
            refresh_token: None,
        })
        .unwrap();
    store.record_url(&url, &idp.auth_config()).unwrap();
    let mut c = Command::new(env!("CARGO_BIN_EXE_nfi-admin"));
    c.args(["--url", &url, "ingest", "https://x/a.nfib", "https://x/b.nfib"])
        .env("XDG_CONFIG_HOME", d.path())
        .env("NFI_TOKEN_CACHE", d.path().join("tokens"))
        .env_remove("NFI_ADMIN_TOKEN")
        .env_remove("NFI_ADMIN_TOKEN_FILE");
    let o = tokio::task::spawn_blocking(move || {
        nfi_test_support::pty::run(c, 100, 30, std::time::Duration::from_secs(60))
    })
    .await
    .unwrap();

    // The second ingest got a 401: one login, then only it was retried.
    assert_eq!(o.code, Some(0), "{}", o.text);
    assert_eq!(o.text.matches("To log in, open").count(), 1, "{}", o.text);
    assert_eq!(idp.device_logins(), 1);
    assert_eq!(
        *fake.ingested.lock().unwrap(),
        ["https://x/a.nfib", "https://x/b.nfib"]
    );
    assert!(o.text.contains("https://x/b.nfib: job 2"), "{}", o.text);
}

/// A token is not sent over plain HTTP to a host other than loopback unless
/// allowed: the command fails before any request (192.0.2.1 is unroutable).
#[test]
fn insecure_url_is_refused() {
    let d = tempfile::tempdir().unwrap();
    let o = run(
        env!("CARGO_BIN_EXE_nfi-admin"),
        d.path(),
        &[("NFI_ADMIN_TOKEN", "tok")],
        &["--url", "http://192.0.2.1", "jobs"],
    );
    assert_eq!(o.code, 1, "{}", o.stderr);
    assert!(
        o.stderr
            .contains("refusing to use credentials with http://192.0.2.1")
            && o.stderr.contains("--allow-insecure"),
        "{}",
        o.stderr
    );
}
