//! Several sites behind one router (M7.1, ADR 0014): each Host reaches its own database, and
//! nothing of one site is visible on another. Two real databases, one router.

mod common;

use axum::Router;
use axum::http::StatusCode;
use common::{Env, PASSWORD, Reply, Session, get, post, send_to};
use forgebase_db::auth;
use forgebase_http::{Sites, sites_router};
use serde_json::json;

const OTHER_PASSWORD: &str = "a completely different one";

/// Two sites, `a.test` and `b.test`, each with its own database and a user `alice` (Writer):
/// the same username, with the password [`PASSWORD`] on A and [`OTHER_PASSWORD`] on B.
struct Pair {
    a: Env,
    b: Env,
    app: Router,
}

impl Pair {
    async fn new() -> Self {
        let a = Env::new().await;
        let b = Env::new().await;
        a.user("alice", &["Writer"]).await;
        {
            let mut client = b.db.pool.get().await.expect("connection");
            auth::create_user(&mut **client, "alice", OTHER_PASSWORD, &["Writer"])
                .await
                .expect("create user");
        }
        let sites = Sites::configured()
            .site(&["a.test".to_owned()], a.state.clone())
            .expect("a")
            .site(
                &["b.test".to_owned(), "b2.test".to_owned()],
                b.state.clone(),
            )
            .expect("b");
        Self {
            a,
            b,
            app: sites_router(sites),
        }
    }

    async fn send(&self, host: &str, request: common::Req) -> Reply {
        send_to(&self.app, request.host(Some(host))).await
    }

    async fn login(&self, host: &str, password: &str) -> Reply {
        self.send(
            host,
            post("/api/v1/login").json(json!({"username": "alice", "password": password})),
        )
        .await
    }

    async fn session(&self, host: &str, password: &str) -> Session {
        let reply = self.login(host, password).await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
        let cookie = reply.set_cookies().remove(0);
        // The cookie is host-scoped: no `Domain` attribute, so a browser never sends it to
        // another host.
        assert!(!cookie.to_ascii_lowercase().contains("domain"), "{cookie}");
        let token = cookie
            .strip_prefix("fb_session=")
            .and_then(|rest| rest.split(';').next())
            .expect("session cookie")
            .to_owned();
        let csrf = reply.data()["csrf_token"]
            .as_str()
            .expect("csrf")
            .to_owned();
        Session { token, csrf }
    }
}

#[tokio::test]
async fn each_password_works_only_on_its_own_site() {
    let pair = Pair::new().await;
    assert_eq!(pair.login("a.test", PASSWORD).await.status, StatusCode::OK);
    assert_eq!(
        pair.login("b.test", OTHER_PASSWORD).await.status,
        StatusCode::OK
    );
    // Both hosts of site B reach B.
    assert_eq!(
        pair.login("b2.test", OTHER_PASSWORD).await.status,
        StatusCode::OK
    );
    for (host, password) in [("a.test", OTHER_PASSWORD), ("b.test", PASSWORD)] {
        let reply = pair.login(host, password).await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{host}");
        assert_eq!(reply.code(), "unauthenticated");
    }
}

#[tokio::test]
async fn a_session_of_one_site_is_refused_on_the_other() {
    let pair = Pair::new().await;
    let on_a = pair.session("a.test", PASSWORD).await;
    let on_b = pair.session("b.test", OTHER_PASSWORD).await;
    for (host, own) in [("a.test", &on_a), ("b.test", &on_b)] {
        let reply = pair.send(host, get("/api/v1/meta").cookie(own)).await;
        assert_eq!(reply.status, StatusCode::OK, "{host}: {}", reply.text);
    }
    // The cookie of A on B, and the other way round: the user `alice` exists on both, but the
    // session lives in the database of its own site.
    let reply = pair.send("b.test", get("/api/v1/meta").cookie(&on_a)).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    let reply = pair.send("a.test", get("/api/v1/meta").cookie(&on_b)).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    // A write with A's cookie and CSRF token is refused on B before anything is written.
    let reply = pair
        .send(
            "b.test",
            post("/api/v1/resource/note")
                .session(&on_a)
                .json(json!({"title": "x"})),
        )
        .await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    // Logging out on A does not end the session on B.
    let reply = pair
        .send("a.test", post("/api/v1/logout").session(&on_a))
        .await;
    assert_eq!(reply.status, StatusCode::NO_CONTENT);
    let reply = pair.send("b.test", get("/api/v1/meta").cookie(&on_b)).await;
    assert_eq!(reply.status, StatusCode::OK);
}

#[tokio::test]
async fn an_api_token_of_one_site_is_refused_on_the_other() {
    let pair = Pair::new().await;
    let token = pair.a.api_token("alice").await;
    let reply = pair.send("a.test", get("/api/v1/meta").token(&token)).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    let reply = pair.send("b.test", get("/api/v1/meta").token(&token)).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn documents_of_one_site_are_invisible_on_the_other() {
    let pair = Pair::new().await;
    let on_a = pair.session("a.test", PASSWORD).await;
    let on_b = pair.session("b.test", OTHER_PASSWORD).await;
    let created = pair
        .send(
            "a.test",
            post("/api/v1/resource/note")
                .session(&on_a)
                .json(json!({"title": "only on A"})),
        )
        .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.text);
    let name = created.data()["name"].as_str().expect("name").to_owned();

    let list_a = pair
        .send("a.test", get("/api/v1/resource/note").cookie(&on_a))
        .await;
    assert_eq!(list_a.data().as_array().expect("list").len(), 1);
    let list_b = pair
        .send("b.test", get("/api/v1/resource/note").cookie(&on_b))
        .await;
    assert_eq!(list_b.status, StatusCode::OK);
    assert!(list_b.data().as_array().expect("list").is_empty());
    let uri = format!("/api/v1/resource/note/{name}");
    let read_b = pair.send("b.test", get(&uri).cookie(&on_b)).await;
    assert_eq!(read_b.status, StatusCode::NOT_FOUND);
    let read_a = pair.send("a.test", get(&uri).cookie(&on_a)).await;
    assert_eq!(read_a.status, StatusCode::OK);
    // And the data really is in one database only.
    let count = |env: &Env| {
        let pool = env.db.pool.clone();
        async move {
            let client = pool.get().await.expect("connection");
            client
                .query_one("SELECT count(*) FROM doc_note", &[])
                .await
                .expect("count")
                .get::<_, i64>(0)
        }
    };
    assert_eq!((count(&pair.a).await, count(&pair.b).await), (1, 0));
}

#[tokio::test]
async fn the_meta_api_works_on_every_site() {
    let pair = Pair::new().await;
    for (host, password) in [("a.test", PASSWORD), ("b.test", OTHER_PASSWORD)] {
        let session = pair.session(host, password).await;
        let reply = pair
            .send(host, get("/api/v1/meta/note").cookie(&session))
            .await;
        assert_eq!(reply.status, StatusCode::OK, "{host}: {}", reply.text);
        assert_eq!(reply.data()["name"], "note");
    }
}

#[tokio::test]
async fn unknown_hosts_are_404_malformed_ones_400_and_forwarding_headers_are_ignored() {
    let pair = Pair::new().await;
    for host in ["c.test", "localhost", "a.test.evil.test"] {
        let reply = pair.send(host, get("/api/v1/meta")).await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND, "{host}");
        assert_eq!(reply.code(), "not_found");
        assert_eq!(reply.header("cache-control").as_deref(), Some("no-store"));
        // Login on an unknown host is the same 404: no site, no database.
        let reply = pair
            .send(
                host,
                post("/api/v1/login").json(json!({"username": "alice", "password": PASSWORD})),
            )
            .await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND, "{host}");
    }
    for host in ["a.test:80@evil.test", "a b"] {
        let reply = pair.send(host, get("/api/v1/meta")).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{host}");
        assert_eq!(reply.code(), "invalid_host");
    }
    let reply = send_to(&pair.app, get("/api/v1/meta").host(None)).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(reply.code(), "invalid_host");

    // `X-Forwarded-Host` and `Forwarded` do not choose the site: A's password still does not
    // work on B's Host, and an unknown Host stays unknown.
    let forwarded = |request: common::Req| {
        request
            .header("x-forwarded-host", "a.test")
            .header("forwarded", "host=a.test")
    };
    let reply = pair
        .send(
            "b.test",
            forwarded(
                post("/api/v1/login").json(json!({"username": "alice", "password": PASSWORD})),
            ),
        )
        .await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    let reply = pair.send("c.test", forwarded(get("/api/v1/meta"))).await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
}

/// LOW-3 of the M7.1 review: a request target in absolute form must name the host of the `Host`
/// header (case and port aside); a different one is `400 invalid_host`, never a request routed
/// by one host and answered for the other.
#[tokio::test]
async fn an_absolute_form_target_must_agree_with_the_host_header() {
    let pair = Pair::new().await;
    // The same host (case and port ignored) is fine: the request reaches the site.
    for (target, host) in [
        ("http://a.test/api/v1/meta", "a.test"),
        ("http://A.TEST:8000/api/v1/meta", "a.test:9000"),
        ("http://a.test/api/v1/meta", "A.Test"),
    ] {
        let reply = pair.send(host, get(target)).await;
        // Authentication comes after routing: a 401 shows the site was reached.
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{target} {host}");
    }
    // Another host in the target, on a known or an unknown host: refused.
    for (target, host) in [
        ("http://b.test/api/v1/meta", "a.test"),
        ("http://a.test/api/v1/meta", "b.test"),
        ("http://c.test/api/v1/meta", "a.test"),
        ("http://a.test.evil.test/api/v1/meta", "a.test"),
    ] {
        let reply = pair.send(host, get(target)).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{target} {host}");
        assert_eq!(reply.code(), "invalid_host", "{target} {host}");
    }
}
