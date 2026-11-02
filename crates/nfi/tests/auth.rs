//! The query CLI against an OIDC-protected service: explicit tokens, cached
//! logins and the hint shown when a login is needed.

use nfi_auth_client::{StoredToken, TokenStore};
use nfi_auth_server::{Authenticator, Expr, OidcSettings};
use nfi_service::auth::{AdminAuth, ServiceAuth};
use nfi_test_support::{Impostor, MockIdp};
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

    // Never logged in, and no terminal to log in from: the request is sent
    // without a token, and its 401 gets the hint (the service's settings,
    // fetched after it, say it uses login).
    let o = nfi(&[], &["releases"]);
    assert_eq!(o.code, 1);
    assert!(
        o.stderr.contains("run `nfi login`") && o.stderr.contains("401"),
        "{}",
        o.stderr
    );
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

    // A cached login (as `nfi login` leaves it) is used automatically, but
    // only for the URLs it was made for.
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
    assert_eq!(o.code, 1, "{}", o.stdout);
    for u in [url, "http://127.0.0.1:9"] {
        store.record_url(u, &idp.auth_config()).unwrap();
    }
    let o = nfi(&[], &["whoami"]);
    assert!(o.stdout.starts_with(&format!("{url}: bob\n")), "{}", o.stderr);

    // Logging out forgets the login, also for the other URL using it.
    let o = nfi(&[], &["logout"]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(
        o.stderr.contains(&format!("logged out of {url}"))
            && o.stderr
                .contains("also logged out of http://127.0.0.1:9 (same login)"),
        "{}",
        o.stderr
    );
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

    // On a terminal, a command needing a login logs in once the service
    // rejects it (it was never logged in to, so nothing says it needs one
    // before), even with stdin piped (the device flow only uses stderr).
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

/// A service claiming the provider and client of a cached login, at a URL
/// never logged in to, gets no token from any command, with or without a
/// terminal.
#[tokio::test(flavor = "multi_thread")]
async fn cached_login_not_sent_to_other_urls() {
    let idp = MockIdp::start("nfi").await;
    let evil = Impostor::start(&idp).await;
    let d = tempfile::tempdir().unwrap();
    let cached = idp.token(json!({"sub": "cached", "preferred_username": "alice"}));
    let store = TokenStore::at(d.path().join("tokens"));
    store
        .save_token(&StoredToken {
            issuer: idp.issuer().into(),
            client_id: idp.client_id().into(),
            id_token: cached.clone(),
            refresh_token: None,
        })
        .unwrap();
    store
        .record_url("http://nfi.example", &idp.auth_config())
        .unwrap();
    let url = evil.url();

    let o = run(
        env!("CARGO_BIN_EXE_nfi"),
        d.path(),
        &[],
        &["--url", url, "releases"],
    );
    assert_eq!(o.code, 1);
    assert!(o.stderr.contains("run `nfi login`"), "{}", o.stderr);
    let o = run(
        env!("CARGO_BIN_EXE_nfi"),
        d.path(),
        &[],
        &["--url", url, "whoami"],
    );
    assert_eq!(
        (o.code, o.stdout.trim()),
        (1, format!("{url}: not authenticated").as_str())
    );
    assert!(evil.bearers().is_empty());

    // On a terminal, `whoami` still sends nothing, and a command asks for a
    // new login rather than using the cached one.
    let o = nfi_tty(d.path(), &["--url", url, "whoami"]).await;
    assert_eq!(o.code, Some(1), "{}", o.text);
    assert!(!o.text.contains("To log in"), "{}", o.text);
    assert!(evil.bearers().is_empty());
    let o = nfi_tty(d.path(), &["--url", url, "releases"]).await;
    assert_eq!(o.code, Some(0), "{}", o.text);
    assert!(o.text.contains("To log in, open"), "{}", o.text);
    assert_eq!(idp.device_logins(), 1);
    let sent = evil.bearers();
    assert_eq!(sent.len(), 1);
    assert_ne!(sent[0], cached);
}

/// The service's settings are not fetched by commands to a URL logged in
/// to (its record has them), nor to one never logged in to until a 401.
#[tokio::test(flavor = "multi_thread")]
async fn settings_fetched_only_after_a_401() {
    let idp = MockIdp::start("nfi").await;
    let evil = Impostor::start(&idp).await;
    let d = tempfile::tempdir().unwrap();
    let nfi = |args: &[&str]| {
        let mut a = vec!["--url", evil.url()];
        a.extend_from_slice(args);
        run(env!("CARGO_BIN_EXE_nfi"), d.path(), &[], &a)
    };
    let o = nfi(&["whoami"]);
    assert_eq!(o.code, 1, "{}", o.stderr);
    assert_eq!(idp.config_fetches(), 0);
    let o = nfi(&["releases"]);
    assert_eq!(o.code, 1);
    assert!(o.stderr.contains("run `nfi login`"), "{}", o.stderr);
    assert_eq!(idp.config_fetches(), 1);

    let store = TokenStore::at(d.path().join("tokens"));
    let alice = idp.token(json!({"sub": "1", "preferred_username": "alice"}));
    store
        .save_token(&StoredToken {
            issuer: idp.issuer().into(),
            client_id: idp.client_id().into(),
            id_token: alice.clone(),
            refresh_token: None,
        })
        .unwrap();
    store.record_url(evil.url(), &idp.auth_config()).unwrap();
    for args in [&["releases"][..], &["whoami"]] {
        let o = nfi(args);
        assert_eq!(o.code, 0, "{}", o.stderr);
    }
    assert_eq!(idp.config_fetches(), 1);
    assert_eq!(evil.bearers(), [alice.clone(), alice]);
}

/// A URL recorded with settings the service no longer uses (another
/// provider): its 401 fetches the current ones, logs in with them and
/// records them.
#[tokio::test(flavor = "multi_thread")]
async fn changed_settings_are_picked_up_after_a_401() {
    let idp = MockIdp::start("nfi").await;
    let old = MockIdp::start("old").await;
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
    let store = TokenStore::at(d.path().join("tokens"));
    store
        .save_token(&StoredToken {
            issuer: old.issuer().into(),
            client_id: old.client_id().into(),
            id_token: old.token(json!({"sub": "1"})),
            refresh_token: None,
        })
        .unwrap();
    store.record_url(url, &old.auth_config()).unwrap();

    let o = nfi_tty(d.path(), &["--url", url, "releases"]).await;
    assert_eq!(o.code, Some(0), "{}", o.text);
    assert_eq!(o.text.matches("To log in, open").count(), 1, "{}", o.text);
    assert_eq!((idp.device_logins(), old.device_logins()), (1, 0));
    // The record now names the new provider login: used without a terminal.
    let o = run(
        env!("CARGO_BIN_EXE_nfi"),
        d.path(),
        &[],
        &["--url", url, "whoami"],
    );
    assert!(o.stdout.starts_with(&format!("{url}: alice\n")), "{}", o.stderr);
}

/// Tokens are not sent over plain HTTP to hosts other than loopback unless
/// allowed: the command fails before any request (192.0.2.1 is unroutable).
#[test]
fn insecure_urls_are_refused() {
    let d = tempfile::tempdir().unwrap();
    let url = "http://192.0.2.1:8080";
    for args in [&["releases"][..], &["whoami"]] {
        let mut a = vec!["--url", url];
        a.extend_from_slice(args);
        let o = run(env!("CARGO_BIN_EXE_nfi"), d.path(), &[("NFI_TOKEN", "tok")], &a);
        assert_eq!(o.code, 1, "{}", o.stderr);
        assert!(
            o.stderr
                .contains(&format!("refusing to use credentials with {url}"))
                && o.stderr.contains("--allow-insecure or set NFI_ALLOW_INSECURE=1"),
            "{}",
            o.stderr
        );
    }
}
