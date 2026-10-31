//! Protocol-level behavior: the error envelope for every kind of failure, body limits, no CORS,
//! no secrets or internals in responses, against a real PostgreSQL.

mod common;

use axum::http::{Method, StatusCode};
use common::{Env, PASSWORD, Reply, get, post, req};
use forgebase_http::AppConfig;
use serde_json::json;

async fn env_with_writer() -> (Env, common::Session) {
    let env = Env::new().await;
    env.user("writer", &["Writer"]).await;
    let session = env.login("writer").await;
    (env, session)
}

fn assert_envelope(reply: &Reply, status: StatusCode, code: &str) {
    assert_eq!(reply.status, status, "{}", reply.text);
    assert_eq!(
        reply.header("content-type").as_deref(),
        Some("application/json"),
        "{}",
        reply.text
    );
    let body = reply.json();
    let error = body["error"].as_object().expect("error object");
    assert_eq!(error["code"], code, "{}", reply.text);
    assert!(error["message"].is_string());
    assert!(body.as_object().expect("object").len() == 1);
}

#[tokio::test]
async fn every_kind_of_failure_uses_the_envelope() {
    let (env, s) = env_with_writer().await;
    let big = vec![b'a'; 10];
    // Unknown routes, wrong methods.
    for uri in ["/", "/api", "/api/v1", "/api/v1/method", "/api/v1/resource"] {
        assert_envelope(
            &env.send(get(uri).session(&s)).await,
            StatusCode::NOT_FOUND,
            "not_found",
        );
    }
    assert_envelope(
        &env.send(req(Method::PUT, "/api/v1/resource/note").session(&s))
            .await,
        StatusCode::METHOD_NOT_ALLOWED,
        "method_not_allowed",
    );
    // The route of the app methods (M9.1) is POST only. This test environment has no dispatcher
    // and no methods (they come with the `forgebase` crate): a call is the 404 of an unknown
    // method, and without a credential it is the 401 first.
    assert_envelope(
        &env.send(get("/api/v1/method/frappe.ping").session(&s))
            .await,
        StatusCode::METHOD_NOT_ALLOWED,
        "method_not_allowed",
    );
    assert_envelope(
        &env.send(post("/api/v1/method/frappe.ping").json(serde_json::json!({})))
            .await,
        StatusCode::UNAUTHORIZED,
        "unauthenticated",
    );
    assert_envelope(
        &env.send(
            post("/api/v1/method/frappe.ping")
                .session(&s)
                .json(serde_json::json!({})),
        )
        .await,
        StatusCode::NOT_FOUND,
        "not_found",
    );
    assert_envelope(
        &env.send(get("/api/v1/login")).await,
        StatusCode::METHOD_NOT_ALLOWED,
        "method_not_allowed",
    );
    assert_envelope(
        &env.send(req(Method::OPTIONS, "/api/v1/resource/note"))
            .await,
        StatusCode::METHOD_NOT_ALLOWED,
        "method_not_allowed",
    );
    // Malformed JSON, another content type, no content type, a path that is not UTF-8.
    assert_envelope(
        &env.send(
            post("/api/v1/resource/note")
                .session(&s)
                .raw("application/json", b"{"),
        )
        .await,
        StatusCode::BAD_REQUEST,
        "bad_request",
    );
    assert_envelope(
        &env.send(
            post("/api/v1/resource/note")
                .session(&s)
                .raw("application/json", b""),
        )
        .await,
        StatusCode::BAD_REQUEST,
        "bad_request",
    );
    assert_envelope(
        &env.send(
            post("/api/v1/resource/note")
                .session(&s)
                .raw("text/plain", &big),
        )
        .await,
        StatusCode::UNSUPPORTED_MEDIA_TYPE,
        "unsupported_media_type",
    );
    assert_envelope(
        &env.send(post("/api/v1/resource/note").session(&s)).await,
        StatusCode::UNSUPPORTED_MEDIA_TYPE,
        "unsupported_media_type",
    );
    assert_envelope(
        &env.send(post("/api/v1/login").raw("application/json", b"["))
            .await,
        StatusCode::BAD_REQUEST,
        "bad_request",
    );
    assert_envelope(
        &env.send(get("/api/v1/resource/%FF").session(&s)).await,
        StatusCode::BAD_REQUEST,
        "bad_request",
    );
    assert_envelope(
        &env.send(get("/api/v1/resource/note?limit=%FF").session(&s))
            .await,
        StatusCode::BAD_REQUEST,
        "invalid_query",
    );
    // Authentication comes before the body: no credential and a bad body is a 401.
    assert_envelope(
        &env.send(post("/api/v1/resource/note").raw("application/json", b"{"))
            .await,
        StatusCode::UNAUTHORIZED,
        "unauthenticated",
    );
    assert_envelope(
        &env.send(post("/api/v1/resource/note").raw("text/plain", &big))
            .await,
        StatusCode::UNAUTHORIZED,
        "unauthenticated",
    );
}

#[tokio::test]
async fn the_body_limit_is_413_in_the_envelope() {
    let env = Env::with_config(AppConfig {
        body_limit_bytes: 256,
        ..AppConfig::default()
    })
    .await;
    env.user("writer", &["Writer"]).await;
    let s = env.login("writer").await;
    let small = json!({"title": "ok"});
    let reply = env
        .send(post("/api/v1/resource/note").session(&s).json(small))
        .await;
    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.text);
    let large = json!({"title": "x".repeat(1000)});
    let reply = env
        .send(
            post("/api/v1/resource/note")
                .session(&s)
                .json(large.clone()),
        )
        .await;
    assert_envelope(&reply, StatusCode::PAYLOAD_TOO_LARGE, "payload_too_large");
    // On login too, and for PATCH.
    let reply = env
        .send(
            post("/api/v1/login").json(json!({"username": "writer", "password": "x".repeat(1000)})),
        )
        .await;
    assert_envelope(&reply, StatusCode::PAYLOAD_TOO_LARGE, "payload_too_large");
    let reply = env
        .send(
            req(Method::PATCH, "/api/v1/resource/note/x")
                .session(&s)
                .json(large),
        )
        .await;
    assert_envelope(&reply, StatusCode::PAYLOAD_TOO_LARGE, "payload_too_large");
}

#[tokio::test]
async fn no_cors_headers_anywhere() {
    let (env, s) = env_with_writer().await;
    let token = env.api_token("writer").await;
    let origin = ("origin", "https://evil.example");
    let replies = [
        env.send(
            get("/api/v1/resource/note")
                .session(&s)
                .header(origin.0, origin.1),
        )
        .await,
        env.send(get("/api/v1/resource/note").header(origin.0, origin.1))
            .await,
        env.send(
            get("/api/v1/resource/nope")
                .token(&token)
                .header(origin.0, origin.1),
        )
        .await,
        env.send(
            post("/api/v1/login")
                .header(origin.0, origin.1)
                .json(json!({"username": "writer", "password": PASSWORD})),
        )
        .await,
        env.send(
            post("/api/v1/login")
                .header(origin.0, origin.1)
                .json(json!({"username": "writer", "password": "wrong password"})),
        )
        .await,
        env.send(
            req(Method::OPTIONS, "/api/v1/resource/note")
                .header(origin.0, origin.1)
                .header("access-control-request-method", "POST")
                .header("access-control-request-headers", "x-forgebase-csrf"),
        )
        .await,
        env.send(get("/api/v1/nothing").header(origin.0, origin.1))
            .await,
        env.send(
            get("/api/v1/nothing")
                .host(Some("evil.example"))
                .header(origin.0, origin.1),
        )
        .await,
    ];
    for reply in env.log.lock().expect("log").iter().chain(&replies) {
        for name in reply.headers.keys() {
            assert!(
                !name.as_str().starts_with("access-control-"),
                "{name} in {}",
                reply.text
            );
        }
        assert!(
            reply
                .header("vary")
                .is_none_or(|v| !v.to_ascii_lowercase().contains("origin"))
        );
    }
    // Every response is also marked uncacheable and not sniffable.
    for reply in &replies {
        assert_eq!(reply.header("cache-control").as_deref(), Some("no-store"));
        assert_eq!(
            reply.header("x-content-type-options").as_deref(),
            Some("nosniff")
        );
    }
}

#[tokio::test]
async fn no_secret_appears_in_a_response_except_where_it_is_issued() {
    let env = Env::new().await;
    env.user("writer", &["Writer"]).await;
    env.user("admin", &["Administrator"]).await;
    let login = env.try_login("writer", PASSWORD).await;
    let session_token = login.set_cookies()[0]
        .strip_prefix("fb_session=")
        .and_then(|rest| rest.split(';').next())
        .expect("cookie")
        .to_owned();
    let csrf = login.data()["csrf_token"]
        .as_str()
        .expect("csrf")
        .to_owned();
    let s = common::Session {
        token: session_token.clone(),
        csrf: csrf.clone(),
    };
    let api = env.api_token("writer").await;
    let (api_key, api_secret) = api.split_once(':').expect("pair");

    // A mixed bag of successful and failing requests.
    let created = env
        .send(
            post("/api/v1/resource/note")
                .session(&s)
                .json(json!({"title": "t"})),
        )
        .await;
    let name = created.data()["name"].as_str().expect("name").to_owned();
    env.send(get("/api/v1/resource/note").session(&s)).await;
    env.send(get(&format!("/api/v1/resource/note/{name}")).token(&api))
        .await;
    env.send(
        post("/api/v1/resource/note")
            .token(&api)
            .json(json!({"nope": 1})),
    )
    .await;
    env.send(
        post("/api/v1/resource/note")
            .cookie(&s)
            .json(json!({"title": "t"})),
    )
    .await;
    env.send(get("/api/v1/resource/note").token(&format!("{api_key}:{}", "0".repeat(64))))
        .await;
    env.send(
        post("/api/v1/resource/secret")
            .session(&s)
            .json(json!({"x": "y"})),
    )
    .await;
    env.send(env_login_attempt("writer", "wrong password"))
        .await;
    env.send(post("/api/v1/logout").session(&s)).await;
    env.send(get("/api/v1/resource/note").session(&s)).await;

    let log = env.log.lock().expect("log");
    for reply in log.iter() {
        for secret in [PASSWORD, "wrong password", api_secret, "postgres://"] {
            assert!(!reply.text.contains(secret), "{secret} in {}", reply.text);
            for (name, value) in &reply.headers {
                assert!(
                    !value.to_str().unwrap_or_default().contains(secret),
                    "{secret} in header {name}"
                );
            }
        }
        // The session token only in Set-Cookie; the CSRF token only in the login body.
        assert!(!reply.text.contains(&session_token), "{}", reply.text);
        for (name, value) in &reply.headers {
            if name != "set-cookie" {
                assert!(!value.to_str().unwrap_or_default().contains(&session_token));
            }
        }
        let is_login = reply.text.contains(&csrf);
        if is_login {
            assert_eq!(reply.data()["csrf_token"], csrf);
        }
        // The API key is not a secret but nothing needs to echo it either.
        assert!(!reply.text.contains(api_key), "{}", reply.text);
    }
    assert_eq!(log.iter().filter(|r| r.text.contains(&csrf)).count(), 1);
}

fn env_login_attempt(username: &str, password: &str) -> common::Req {
    post("/api/v1/login").json(json!({"username": username, "password": password}))
}

#[tokio::test]
async fn internal_errors_are_generic() {
    let (env, s) = env_with_writer().await;
    // Break the storage of a DocType behind the API's back.
    env.db
        .exec("ALTER TABLE doc_note RENAME TO doc_note_gone")
        .await;
    let reply = env.send(get("/api/v1/resource/note").session(&s)).await;
    assert_envelope(&reply, StatusCode::INTERNAL_SERVER_ERROR, "internal");
    assert_eq!(reply.json()["error"]["message"], "internal server error");
    assert!(!reply.text.contains("doc_note"));
    assert!(!reply.text.contains("relation"));
    assert!(!reply.text.contains("SQLSTATE"));
    assert!(!reply.text.contains("postgres"));
    let reply = env
        .send(
            post("/api/v1/resource/note")
                .session(&s)
                .json(json!({"title": "t"})),
        )
        .await;
    assert_envelope(&reply, StatusCode::INTERNAL_SERVER_ERROR, "internal");
    assert!(!reply.text.contains("doc_note"));
    env.db
        .exec("ALTER TABLE doc_note_gone RENAME TO doc_note")
        .await;
}

#[tokio::test]
async fn a_unique_violation_does_not_echo_the_key() {
    let env = Env::new().await;
    env.user("clerk", &["Clerk"]).await;
    let clerk = env.api_token("clerk").await;
    let body = json!({"code": "dup-key-1"});
    let first = env
        .send(
            post("/api/v1/resource/item")
                .token(&clerk)
                .json(body.clone()),
        )
        .await;
    assert_eq!(first.status, StatusCode::CREATED, "{}", first.text);
    let second = env
        .send(post("/api/v1/resource/item").token(&clerk).json(body))
        .await;
    assert_envelope(&second, StatusCode::CONFLICT, "unique_violation");
    assert!(!second.text.contains("doc_item"), "{}", second.text);
    assert!(!second.text.contains("dup-key-1"), "{}", second.text);
}
