//! Login, sessions, CSRF, API tokens and Host validation over HTTP, against a real PostgreSQL.

mod common;

use axum::http::{Method, StatusCode};
use common::{Env, PASSWORD, get, post, req};
use forgebase_http::AppConfig;
use serde_json::json;

async fn env_with_users() -> Env {
    let env = Env::new().await;
    env.user("alice", &["Writer"]).await;
    env.user("bob", &["Writer"]).await;
    env
}

#[tokio::test]
async fn login_sets_the_cookie_with_the_exact_attributes() {
    let env = env_with_users().await;
    let reply = env.try_login("alice", PASSWORD).await;
    assert_eq!(reply.status, StatusCode::OK);
    let cookies = reply.set_cookies();
    assert_eq!(cookies.len(), 1);
    let cookie = &cookies[0];
    let (value, attributes) = cookie.split_once("; ").expect("attributes");
    let token = value.strip_prefix("fb_session=").expect("cookie name");
    assert_eq!(token.len(), 64);
    assert!(token.bytes().all(|b| b.is_ascii_hexdigit()));
    assert_eq!(
        attributes,
        "HttpOnly; Secure; SameSite=Lax; Path=/; Max-Age=604800"
    );
    let data = reply.data();
    assert_eq!(data["username"], "alice");
    assert_eq!(data["roles"], json!(["Writer"]));
    assert_eq!(data["csrf_token"].as_str().expect("csrf").len(), 64);
    // Nothing else in the body: neither the password nor the session token.
    assert_eq!(data.as_object().expect("object").len(), 3);
    assert!(!reply.text.contains(token));
    assert!(!reply.text.contains(PASSWORD));
    assert_eq!(reply.header("cache-control").as_deref(), Some("no-store"));
}

#[tokio::test]
async fn dev_mode_omits_secure() {
    let env = Env::with_config(AppConfig {
        secure_cookies: false,
        ..AppConfig::default()
    })
    .await;
    env.user("alice", &["Writer"]).await;
    let reply = env.try_login("alice", PASSWORD).await;
    let cookie = reply.set_cookies().remove(0);
    assert!(
        cookie.ends_with("; HttpOnly; SameSite=Lax; Path=/; Max-Age=604800"),
        "{cookie}"
    );
    assert!(!cookie.contains("Secure"));
}

#[tokio::test]
async fn a_failed_login_is_the_same_401_for_every_reason() {
    let env = env_with_users().await;
    env.user("carol", &[]).await;
    env.db
        .exec("UPDATE fb_user SET enabled = false WHERE username = 'carol'")
        .await;
    let wrong = env.try_login("alice", "wrong password").await;
    let unknown = env.try_login("nobody", PASSWORD).await;
    let disabled = env.try_login("carol", PASSWORD).await;
    let malformed = env.try_login("", "x").await;
    for reply in [&wrong, &unknown, &disabled, &malformed] {
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
        assert!(reply.set_cookies().is_empty());
    }
    assert_eq!(wrong.text, unknown.text);
    assert_eq!(wrong.text, disabled.text);
    assert_eq!(wrong.text, malformed.text);
    assert_eq!(wrong.code(), "unauthenticated");
}

#[tokio::test]
async fn lockout_works_over_http() {
    let env = env_with_users().await;
    for _ in 0..5 {
        let reply = env.try_login("alice", "wrong password").await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    }
    // Locked: even the right password fails, with the very same answer.
    let locked = env.try_login("alice", PASSWORD).await;
    let wrong = env.try_login("alice", "wrong password").await;
    assert_eq!(locked.status, StatusCode::UNAUTHORIZED);
    assert_eq!(locked.text, wrong.text);
    // Other users are not affected.
    assert_eq!(env.try_login("bob", PASSWORD).await.status, StatusCode::OK);
}

#[tokio::test]
async fn login_bodies_that_are_not_credentials_are_400() {
    let env = env_with_users().await;
    for body in [
        json!({}),
        json!({"username": "alice"}),
        json!({"username": 1, "password": "x"}),
        json!({"username": "alice", "password": "x", "extra": 1}),
        json!([]),
    ] {
        let reply = env.send(post("/api/v1/login").json(body)).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{}", reply.text);
        assert_eq!(reply.code(), "bad_request");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_logins_are_bounded() {
    let env = std::sync::Arc::new(
        Env::with_config(AppConfig {
            max_concurrent_logins: 1,
            ..AppConfig::default()
        })
        .await,
    );
    env.user("alice", &["Writer"]).await;
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let env = env.clone();
        tasks.push(tokio::spawn(async move {
            env.try_login("alice", PASSWORD).await
        }));
    }
    let mut ok = 0;
    let mut busy = 0;
    for task in tasks {
        let reply = task.await.expect("task");
        match reply.status {
            StatusCode::OK => ok += 1,
            StatusCode::SERVICE_UNAVAILABLE => {
                busy += 1;
                assert_eq!(reply.code(), "too_many_logins");
                assert_eq!(reply.header("retry-after").as_deref(), Some("1"));
                assert!(reply.set_cookies().is_empty());
            }
            other => panic!("unexpected {other}: {}", reply.text),
        }
    }
    assert!(ok >= 1, "at least one login runs");
    assert!(busy >= 1, "the excess is turned away");
    // The permit is free again afterwards.
    assert_eq!(
        env.try_login("alice", PASSWORD).await.status,
        StatusCode::OK
    );
}

#[tokio::test]
async fn a_cookie_session_reads_and_needs_csrf_to_change_things() {
    let env = env_with_users().await;
    let alice = env.login("alice").await;
    let bob = env.login("bob").await;
    let body = json!({"title": "t"});
    // GET needs no CSRF.
    let reply = env.send(get("/api/v1/resource/note").cookie(&alice)).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    // POST without, with a wrong, and with another session's CSRF token.
    let without = env
        .send(
            post("/api/v1/resource/note")
                .cookie(&alice)
                .json(body.clone()),
        )
        .await;
    let wrong = env
        .send(
            post("/api/v1/resource/note")
                .cookie(&alice)
                .csrf(&"0".repeat(64))
                .json(body.clone()),
        )
        .await;
    let other = env
        .send(
            post("/api/v1/resource/note")
                .cookie(&alice)
                .csrf(&bob.csrf)
                .json(body.clone()),
        )
        .await;
    let garbage = env
        .send(
            post("/api/v1/resource/note")
                .cookie(&alice)
                .csrf("not hex")
                .json(body.clone()),
        )
        .await;
    // A repeated header is refused even when every value is the right one.
    let repeated = env
        .send(
            post("/api/v1/resource/note")
                .cookie(&alice)
                .csrf(&alice.csrf)
                .csrf(&alice.csrf)
                .json(body.clone()),
        )
        .await;
    for reply in [&without, &wrong, &other, &garbage, &repeated] {
        assert_eq!(reply.status, StatusCode::FORBIDDEN, "{}", reply.text);
        assert_eq!(reply.code(), "csrf_failed");
    }
    // Nothing was created.
    let list = env.send(get("/api/v1/resource/note").cookie(&alice)).await;
    assert_eq!(list.data(), json!([]));
    // The right token works, and PATCH and DELETE need it too.
    let created = env
        .send(post("/api/v1/resource/note").session(&alice).json(body))
        .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.text);
    let name = created.data()["name"].as_str().expect("name").to_owned();
    let uri = format!("/api/v1/resource/note/{name}");
    let patch = json!({"version": 1, "data": {"body": "x"}});
    let reply = env
        .send(req(Method::PATCH, &uri).cookie(&alice).json(patch.clone()))
        .await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    assert_eq!(reply.code(), "csrf_failed");
    let reply = env
        .send(req(Method::PATCH, &uri).session(&alice).json(patch))
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    let reply = env
        .send(req(Method::DELETE, &format!("{uri}?version=2")).cookie(&alice))
        .await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    let reply = env
        .send(req(Method::DELETE, &format!("{uri}?version=2")).session(&alice))
        .await;
    assert_eq!(reply.status, StatusCode::NO_CONTENT, "{}", reply.text);
    assert_eq!(reply.text, "");
}

#[tokio::test]
async fn logout_ends_the_session_and_clears_the_cookie() {
    let env = env_with_users().await;
    let alice = env.login("alice").await;
    // CSRF is required.
    let reply = env.send(post("/api/v1/logout").cookie(&alice)).await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    assert_eq!(
        env.send(get("/api/v1/resource/note").cookie(&alice))
            .await
            .status,
        StatusCode::OK
    );
    let reply = env.send(post("/api/v1/logout").session(&alice)).await;
    assert_eq!(reply.status, StatusCode::NO_CONTENT);
    assert_eq!(
        reply.set_cookies(),
        ["fb_session=; HttpOnly; Secure; SameSite=Lax; Path=/; Max-Age=0"]
    );
    let after = env.send(get("/api/v1/resource/note").cookie(&alice)).await;
    assert_eq!(after.status, StatusCode::UNAUTHORIZED);
    // A second logout with the dead session is a 401 like any request.
    let again = env.send(post("/api/v1/logout").session(&alice)).await;
    assert_eq!(again.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn expired_unknown_and_tampered_cookies_are_the_same_401() {
    let env = env_with_users().await;
    let alice = env.login("alice").await;
    let tampered = {
        let mut t = alice.token.clone();
        let last = t.pop().expect("char");
        t.push(if last == '0' { '1' } else { '0' });
        t
    };
    let mut replies = vec![
        env.send(get("/api/v1/resource/note")).await,
        env.send(get("/api/v1/resource/note").cookie_value(&tampered))
            .await,
        env.send(get("/api/v1/resource/note").cookie_value("garbage"))
            .await,
        env.send(get("/api/v1/resource/note").cookie_value(""))
            .await,
    ];
    env.db
        .exec("UPDATE fb_session SET expires_at = now() - interval '1 second'")
        .await;
    replies.push(env.send(get("/api/v1/resource/note").cookie(&alice)).await);
    for reply in &replies {
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
        assert_eq!(reply.text, replies[0].text);
    }
    assert_eq!(replies[0].code(), "unauthenticated");
    // Idle for more than 24 hours is expired too.
    let bob = env.login("bob").await;
    env.db
        .exec("UPDATE fb_session SET last_seen_at = now() - interval '25 hours'")
        .await;
    let reply = env.send(get("/api/v1/resource/note").cookie(&bob)).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_disabled_user_loses_the_session() {
    let env = env_with_users().await;
    let alice = env.login("alice").await;
    env.db
        .exec("UPDATE fb_user SET enabled = false WHERE username = 'alice'")
        .await;
    let reply = env.send(get("/api/v1/resource/note").cookie(&alice)).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn an_api_token_needs_no_csrf() {
    let env = env_with_users().await;
    let token = env.api_token("alice").await;
    let created = env
        .send(
            post("/api/v1/resource/note")
                .token(&token)
                .json(json!({"title": "t"})),
        )
        .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.text);
    assert_eq!(created.data()["owner"], "alice");
    let reply = env.send(get("/api/v1/resource/note").token(&token)).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.data().as_array().expect("list").len(), 1);
    // The scheme is case-insensitive.
    let reply = env
        .send(get("/api/v1/resource/note").header("authorization", &format!("TOKEN {token}")))
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    // Logout is for sessions.
    let reply = env.send(post("/api/v1/logout").token(&token)).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(reply.code(), "bad_request");
}

#[tokio::test]
async fn bad_tokens_are_the_same_401() {
    let env = env_with_users().await;
    let token = env.api_token("alice").await;
    let (key, secret) = token.split_once(':').expect("key:secret");
    let revoked = env.api_token("alice").await;
    let client = env.db.pool.get().await.expect("connection");
    forgebase_db::auth::revoke_api_token(&**client, revoked.split_once(':').expect("pair").0)
        .await
        .expect("revoke");
    let wrong_secret = format!("{key}:{}", "0".repeat(64));
    let headers = [
        format!("token {wrong_secret}"),
        format!("token {revoked}"),
        format!("token {key}"),
        format!("token :{secret}"),
        "token".to_owned(),
        "token ".to_owned(),
        format!("Bearer {token}"),
        format!("Basic {token}"),
        token.clone(),
        String::new(),
    ];
    let mut texts = Vec::new();
    for header in &headers {
        let reply = env
            .send(get("/api/v1/resource/note").header("authorization", header))
            .await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{header:?}");
        texts.push(reply.text);
    }
    assert!(texts.iter().all(|t| *t == texts[0]));
    // A header that is not valid text.
    let reply = env
        .send(get("/api/v1/resource/note").header("authorization", "token \u{e9}"))
        .await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn the_authorization_header_wins_over_the_cookie() {
    let env = env_with_users().await;
    let alice = env.login("alice").await;
    let bob_token = env.api_token("bob").await;
    // A bad header is a 401 even with a good session, with CSRF too.
    let reply = env
        .send(
            post("/api/v1/resource/note")
                .session(&alice)
                .header("authorization", "token nonsense")
                .json(json!({"title": "t"})),
        )
        .await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    // A good token with a bad cookie works and acts as the token's user, without CSRF.
    let reply = env
        .send(
            post("/api/v1/resource/note")
                .cookie_value("garbage")
                .token(&bob_token)
                .json(json!({"title": "t"})),
        )
        .await;
    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.text);
    // With both, the token's user is the caller, not the cookie's.
    let reply = env
        .send(
            post("/api/v1/resource/note")
                .cookie(&alice)
                .token(&bob_token)
                .json(json!({"title": "t2"})),
        )
        .await;
    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.text);
    assert_eq!(reply.data()["owner"], "bob");
}

#[tokio::test]
async fn the_host_is_checked_first_on_every_route() {
    let env = env_with_users().await;
    let alice = env.login("alice").await;
    let login_body = json!({"username": "alice", "password": PASSWORD});
    let hosts: [Option<&str>; 6] = [
        None,
        Some("evil.example"),
        Some("localhost.evil.example"),
        Some("localhost:80@evil.example"),
        Some(""),
        Some("127.0.0.2"),
    ];
    for host in hosts {
        let requests = [
            post("/api/v1/login").json(login_body.clone()),
            get("/api/v1/resource/note").session(&alice),
            post("/api/v1/logout").session(&alice),
            get("/api/v1/nothing"),
            req(Method::PUT, "/api/v1/login"),
            // Not even a bad body or a bad credential gets past: the Host comes first.
            post("/api/v1/login").raw("text/plain", b"{"),
        ];
        for request in requests {
            let reply = env.send(request.host(host)).await;
            assert_eq!(
                reply.status,
                StatusCode::BAD_REQUEST,
                "{host:?}: {}",
                reply.text
            );
            assert_eq!(reply.code(), "invalid_host");
            assert!(reply.set_cookies().is_empty());
        }
    }
    // The session was not touched by the refused requests.
    let reply = env.send(get("/api/v1/resource/note").cookie(&alice)).await;
    assert_eq!(reply.status, StatusCode::OK);
    // Port and case are ignored; the other configured host works.
    for host in ["LOCALHOST", "localhost:8000", "127.0.0.1", "127.0.0.1:3000"] {
        let reply = env
            .send(get("/api/v1/resource/note").cookie(&alice).host(Some(host)))
            .await;
        assert_eq!(reply.status, StatusCode::OK, "{host}");
    }
}

#[tokio::test]
async fn a_repeated_host_header_is_refused() {
    let env = env_with_users().await;
    let reply = env
        .send(get("/api/v1/nothing").header("host", "localhost"))
        .await;
    // `send` adds one Host and the request adds a second.
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(reply.code(), "invalid_host");
}

#[tokio::test]
async fn configured_hosts_are_used_and_ipv6_works() {
    let env = Env::with_config(AppConfig {
        allowed_hosts: vec!["Erp.Example.com".to_owned(), "[::1]".to_owned()],
        ..AppConfig::default()
    })
    .await;
    for (host, status) in [
        ("erp.example.com", StatusCode::NOT_FOUND),
        ("ERP.EXAMPLE.COM:443", StatusCode::NOT_FOUND),
        ("[::1]:8000", StatusCode::NOT_FOUND),
        ("localhost", StatusCode::BAD_REQUEST),
    ] {
        let reply = env.send(get("/api/v1/nothing").host(Some(host))).await;
        assert_eq!(reply.status, status, "{host}");
    }
}
