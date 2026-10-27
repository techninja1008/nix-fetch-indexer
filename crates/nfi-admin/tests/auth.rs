//! The admin CLI against an OIDC-protected admin listener.

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
        admin: AdminAuth::Oidc(Expr::new("user.username == 'root'").unwrap()),
    };
    let Some(svc) = nfi_testenv::start_service_with(auth).await else {
        return;
    };
    let d = tempfile::tempdir().unwrap();
    let store = TokenStore::at(d.path().join("tokens"));
    let cfg: nfi_auth::AuthConfig = reqwest::get(format!("{}{}", svc.admin_url, nfi_auth::CONFIG_PATH))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    // A login made with `nfi login` for the public listener.
    store.save_service(&svc.public_url, &cfg).unwrap();
    store
        .save_token(&StoredToken {
            issuer: cfg.issuer.clone(),
            client_id: cfg.client_id.clone(),
            id_token: idp.token(json!({"sub": "2", "preferred_username": "bob"})),
            refresh_token: None,
        })
        .unwrap();

    // The admin CLI shares the login (same provider and client) once it
    // knows the admin listener uses it; bob fails the admin policy.
    let admin = |env: &[(&str, &str)], args: &[&str]| {
        let mut a = vec!["--url", svc.admin_url.as_str()];
        a.extend_from_slice(args);
        run(env!("CARGO_BIN_EXE_nfi-admin"), d.path(), env, &a)
    };
    let o = admin(&[], &["jobs"]);
    assert_eq!(o.code, 1);
    assert!(o.stderr.contains("run `nfi-admin login`"), "{}", o.stderr);
    store.save_service(&svc.admin_url, &cfg).unwrap();
    let o = admin(&[], &["jobs"]);
    assert_eq!(o.code, 1);
    assert!(o.stderr.contains("403"), "{}", o.stderr);
    let root = idp.token(json!({"sub": "0", "preferred_username": "root"}));
    let o = admin(&[("NFI_ADMIN_TOKEN", &root)], &["jobs"]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    let o = admin(&[("NFI_ADMIN_TOKEN", &root)], &["whoami"]);
    assert_eq!(o.stdout.trim(), "root");
}
