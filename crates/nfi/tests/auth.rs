//! The query CLI against an OIDC-protected service: explicit tokens, cached
//! logins and the hint shown when a login is needed.

use nfi_auth::client::{StoredToken, TokenStore};
use nfi_auth::server::{Authenticator, Expr};
use nfi_auth::testing::MockIdp;
use nfi_service::auth::{AdminAuth, ServiceAuth};
use serde_json::json;
use std::path::Path;
use std::process::{Command, Stdio};

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
    let auth = ServiceAuth {
        oidc: Some(Authenticator::new(idp.settings()).unwrap()),
        allow_anonymous: false,
        main_authz: Expr::new("true").unwrap(),
        admin: AdminAuth::Open,
    };
    let Some(svc) = nfi_testenv::start_service_with(auth).await else {
        return;
    };
    let d = tempfile::tempdir().unwrap();
    let nfi = |env: &[(&str, &str)], args: &[&str]| {
        let mut a = vec!["--url", svc.public_url.as_str()];
        a.extend_from_slice(args);
        run(env!("CARGO_BIN_EXE_nfi"), d.path(), env, &a)
    };

    // Not logged in, and no terminal to log in from.
    let o = nfi(&[], &["releases"]);
    assert_eq!(o.code, 1);
    assert!(o.stderr.contains("run `nfi login`"), "{}", o.stderr);
    assert!(o.stderr.contains("401"), "{}", o.stderr);

    // An explicit token, from the environment or a file.
    let alice = idp.token(json!({"sub": "1", "preferred_username": "alice"}));
    let o = nfi(&[("NFI_TOKEN", &alice)], &["releases"]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    let o = nfi(&[("NFI_TOKEN", &alice)], &["whoami"]);
    assert_eq!(o.stdout.trim(), "alice");
    let f = d.path().join("tok");
    std::fs::write(&f, format!("{alice}\n")).unwrap();
    let o = nfi(&[], &["--token-file", f.to_str().unwrap(), "whoami", "--json"]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&o.stdout).unwrap()["username"],
        "alice"
    );
    // A rejected explicit token is not replaced by a login.
    let o = nfi(&[("NFI_TOKEN", "garbage")], &["releases"]);
    assert_eq!(o.code, 1);
    assert!(!o.stderr.contains("nfi login"), "{}", o.stderr);

    // A cached login (as `nfi login` leaves it) is used automatically.
    let store = TokenStore::at(d.path().join("tokens"));
    let cfg: nfi_auth::AuthConfig = reqwest::get(format!("{}{}", svc.public_url, nfi_auth::CONFIG_PATH))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    store.save_service(&svc.public_url, &cfg).unwrap();
    store
        .save_token(&StoredToken {
            issuer: cfg.issuer.clone(),
            client_id: cfg.client_id.clone(),
            id_token: idp.token(json!({"sub": "2", "preferred_username": "bob"})),
            refresh_token: None,
        })
        .unwrap();
    let o = nfi(&[], &["whoami"]);
    assert_eq!(o.stdout.trim(), "bob", "{}", o.stderr);

    // Logging out forgets the login.
    let o = nfi(&[], &["logout"]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stderr.contains("logged out"), "{}", o.stderr);
    let o = nfi(&[], &["whoami"]);
    assert_eq!(o.code, 1);
    let o = nfi(&[], &["logout"]);
    assert!(o.stderr.contains("not logged in"), "{}", o.stderr);
}
