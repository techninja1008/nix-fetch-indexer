//! The query CLI against an OIDC-protected service: explicit tokens, cached
//! logins and the hint shown when a login is needed.

use nfi_auth_client::{StoredToken, TokenStore};
use nfi_auth_server::{Authenticator, Expr, OidcSettings};
use nfi_service::auth::{AdminAuth, ServiceAuth};
use nfi_test_support::MockIdp;
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
    let auth = ServiceAuth::oidc(
        std::sync::Arc::new(Authenticator::new(OidcSettings::new(idp.issuer(), idp.client_id())).unwrap()),
        Expr::new("true").unwrap(),
        false,
        AdminAuth::Open,
    );
    let Some(svc) = nfi_testenv::start_service_with(auth).await else {
        return;
    };
    let d = tempfile::tempdir().unwrap();
    let nfi = |env: &[(&str, &str)], args: &[&str]| {
        let mut a = vec!["--url", svc.public_url.as_str()];
        a.extend_from_slice(args);
        run(env!("CARGO_BIN_EXE_nfi"), d.path(), env, &a)
    };

    let url = svc.public_url.as_str();

    // Not logged in, and no terminal to log in from: the command stops
    // before sending anything.
    let o = nfi(&[], &["releases"]);
    assert_eq!(o.code, 1);
    assert!(o.stderr.contains("run `nfi login`"), "{}", o.stderr);
    assert!(!o.stderr.contains("401"), "{}", o.stderr);
    let o = nfi(&[], &["whoami"]);
    assert_eq!(
        (o.code, o.stdout.trim()),
        (1, format!("{url}: not authenticated").as_str())
    );

    // An explicit token, from the environment or a file.
    let alice = idp.token(json!({"sub": "1", "preferred_username": "alice"}));
    let o = nfi(&[("NFI_TOKEN", &alice)], &["releases"]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    let o = nfi(&[("NFI_TOKEN", &alice)], &["whoami"]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(
        o.stdout.starts_with(&format!("{url}: alice\n  {{\n")),
        "{}",
        o.stdout
    );
    assert!(
        o.stdout.contains("    \"preferred_username\": \"alice\""),
        "{}",
        o.stdout
    );
    let f = d.path().join("tok");
    std::fs::write(&f, format!("{alice}\n")).unwrap();
    let o = nfi(&[], &["--token-file", f.to_str().unwrap(), "whoami", "--json"]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    let v: serde_json::Value = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(v["username"], "alice");
    assert_eq!(v["additional_info"]["sub"], "1");
    // The token file takes precedence over the environment.
    let o = nfi(
        &[("NFI_TOKEN", "garbage")],
        &["--token-file", f.to_str().unwrap(), "releases"],
    );
    assert_eq!(o.code, 0, "{}", o.stderr);
    // A rejected explicit token is not replaced by a login.
    let o = nfi(&[("NFI_TOKEN", "garbage")], &["releases"]);
    assert_eq!(o.code, 1);
    assert!(o.stderr.contains("the token was rejected"), "{}", o.stderr);
    assert!(!o.stderr.contains("nfi login"), "{}", o.stderr);
    let o = nfi(&[("NFI_TOKEN", "garbage")], &["whoami"]);
    assert_eq!(
        (o.code, o.stdout.trim()),
        (1, format!("{url}: not authenticated").as_str())
    );

    // A cached login (as `nfi login` leaves it) is used automatically.
    let store = TokenStore::at(d.path().join("tokens"));
    store
        .save_token(&StoredToken {
            issuer: idp.issuer().into(),
            client_id: idp.client_id().into(),
            id_token: idp.token(json!({"sub": "2", "preferred_username": "bob"})),
            refresh_token: None,
        })
        .unwrap();
    let o = nfi(&[], &["whoami"]);
    assert!(o.stdout.starts_with(&format!("{url}: bob\n")), "{}", o.stderr);

    // Logging out forgets the login.
    let o = nfi(&[], &["logout"]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stderr.contains("logged out"), "{}", o.stderr);
    let o = nfi(&[], &["whoami"]);
    assert_eq!(o.code, 1);
    let o = nfi(&[], &["logout"]);
    assert!(o.stderr.contains("not logged in"), "{}", o.stderr);
}

/// Run `sh -c script` on a pseudo-terminal, so that `nfi` may prompt for a
/// login; `$NFI` is the binary.
async fn sh_tty(dir: &Path, env: &[(&str, &str)], script: &str) -> nfi_test_support::pty::PtyOutput {
    let mut c = Command::new("sh");
    c.args(["-c", script])
        .env("NFI", env!("CARGO_BIN_EXE_nfi"))
        .env("XDG_CONFIG_HOME", dir)
        .env("NFI_TOKEN_CACHE", dir.join("tokens"))
        .env_remove("NFI_TOKEN")
        .env_remove("NFI_TOKEN_FILE")
        .envs(env.iter().copied());
    tokio::task::spawn_blocking(move || {
        nfi_test_support::pty::run(c, 100, 30, std::time::Duration::from_secs(60))
    })
    .await
    .unwrap()
}

/// Run `nfi` on a pseudo-terminal.
async fn nfi_tty(dir: &Path, args: &[&str]) -> nfi_test_support::pty::PtyOutput {
    let args: Vec<String> = args.iter().map(|a| format!("'{a}'")).collect();
    sh_tty(dir, &[], &format!("exec \"$NFI\" {}", args.join(" "))).await
}

#[tokio::test(flavor = "multi_thread")]
async fn interactive_login() {
    let idp = MockIdp::start("nfi").await;
    let auth = ServiceAuth::oidc(
        std::sync::Arc::new(Authenticator::new(OidcSettings::new(idp.issuer(), idp.client_id())).unwrap()),
        Expr::new("true").unwrap(),
        false,
        AdminAuth::Open,
    );
    let Some(svc) = nfi_testenv::start_service_with(auth).await else {
        return;
    };
    let d = tempfile::tempdir().unwrap();
    let url = svc.public_url.as_str();

    // Argument errors come before any login prompt.
    for (args, err) in [
        (&["bulk"][..], "no items given"),
        (&["bulk", "nonsense"], "expected attr@commit"),
        (&["minimise"], "give installables and/or --file"),
    ] {
        let mut a = vec!["--url", url];
        a.extend_from_slice(args);
        let o = nfi_tty(d.path(), &a).await;
        assert_eq!(o.code, Some(1), "{}", o.text);
        assert!(
            o.text.contains(err) && !o.text.contains("To log in"),
            "{}",
            o.text
        );
    }

    // `whoami` never logs in.
    let o = nfi_tty(d.path(), &["--url", url, "whoami"]).await;
    assert_eq!(o.code, Some(1), "{}", o.text);
    assert!(
        o.text.contains(&format!("{url}: not authenticated")),
        "{}",
        o.text
    );
    assert!(!o.text.contains("To log in"), "{}", o.text);
    assert_eq!(idp.device_logins(), 0);

    // On a terminal, a command needing a login logs in first, even with
    // stdin piped (the device flow only uses stderr).
    let o = sh_tty(
        d.path(),
        &[("URL", url)],
        "echo missing@aaaa | \"$NFI\" --url \"$URL\" bulk -f -",
    )
    .await;
    assert_eq!(o.code, Some(0), "{}", o.text);
    assert!(
        o.text.contains("To log in, open") && o.text.contains("CODE-0"),
        "{}",
        o.text
    );
    assert!(o.text.contains("0 paths in closure"), "{}", o.text);
    assert_eq!(idp.device_logins(), 1);

    // The login is cached: later commands, even without a terminal, use it.
    let o = run(
        env!("CARGO_BIN_EXE_nfi"),
        d.path(),
        &[],
        &["--url", url, "whoami"],
    );
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stdout.starts_with(&format!("{url}: alice\n")), "{}", o.stdout);
    assert!(
        o.stdout.contains("\"preferred_username\": \"alice\""),
        "{}",
        o.stdout
    );

    // `login` reuses a valid cached login, and `--force` starts a new one.
    idp.set_login_claims(json!({"sub": "u2", "preferred_username": "carol"}));
    let o = nfi_tty(d.path(), &["--url", url, "login"]).await;
    assert_eq!(o.code, Some(0), "{}", o.text);
    assert!(
        o.text.contains(&format!("logged in to {url} as alice")),
        "{}",
        o.text
    );
    assert_eq!(idp.device_logins(), 1, "{}", o.text);
    let o = nfi_tty(d.path(), &["--url", url, "login", "--force"]).await;
    assert_eq!(o.code, Some(0), "{}", o.text);
    assert!(o.text.contains("CODE-1"), "{}", o.text);
    assert!(o.text.contains("as carol"), "{}", o.text);
    assert_eq!(idp.device_logins(), 2);

    // An expired login is refreshed without prompting.
    idp.set_token_lifetime(-10);
    let o = nfi_tty(d.path(), &["--url", url, "login", "--force"]).await;
    assert_eq!(o.code, Some(0), "{}", o.text);
    idp.set_token_lifetime(3600);
    let o = nfi_tty(d.path(), &["--url", url, "whoami"]).await;
    assert_eq!(o.code, Some(0), "{}", o.text);
    assert!(!o.text.contains("To log in"), "{}", o.text);
    assert_eq!((idp.device_logins(), idp.refreshes()), (3, 1));

    // A cached login the service rejects: one new login, and the request is
    // made again.
    TokenStore::at(d.path().join("tokens"))
        .save_token(&StoredToken {
            issuer: idp.issuer().into(),
            client_id: idp.client_id().into(),
            id_token: idp.token(json!({"sub": "1", "aud": "someone-else"})),
            refresh_token: None,
        })
        .unwrap();
    let o = nfi_tty(d.path(), &["--url", url, "releases"]).await;
    assert_eq!(o.code, Some(0), "{}", o.text);
    assert_eq!(o.text.matches("To log in, open").count(), 1, "{}", o.text);
    assert_eq!(idp.device_logins(), 4);

    // A rejected explicit token fails with the hint, without prompting.
    let o = sh_tty(
        d.path(),
        &[("URL", url), ("NFI_TOKEN", "garbage")],
        "exec \"$NFI\" --url \"$URL\" releases",
    )
    .await;
    assert_eq!(o.code, Some(1), "{}", o.text);
    assert!(o.text.contains("the token was rejected"), "{}", o.text);
    assert!(!o.text.contains("To log in"), "{}", o.text);
    assert_eq!(idp.device_logins(), 4);
}
