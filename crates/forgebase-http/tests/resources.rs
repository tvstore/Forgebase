//! The resource routes over HTTP: the authorization matrix, `if_owner`, the document lifecycle,
//! decision 9 (no leaks through Links) and injection attempts, against a real PostgreSQL.

mod common;

use axum::http::{Method, StatusCode};
use common::{Env, Session, get, post, req};
use serde_json::{Value as Json, json};

/// An environment with the users the tests need; returns it with the Administrator's session.
async fn setup() -> (Env, Session) {
    let env = Env::new().await;
    env.user("admin", &["Administrator"]).await;
    env.user("reader", &["Reader"]).await;
    env.user("writer", &["Writer"]).await;
    env.user("clerk", &["Clerk"]).await;
    env.user("plain", &[]).await;
    let admin = env.login("admin").await;
    (env, admin)
}

/// Percent-encodes every byte.
fn enc(text: &str) -> String {
    text.bytes().map(|b| format!("%{b:02X}")).collect()
}

fn uri(doctype: &str, name: &str) -> String {
    format!("/api/v1/resource/{doctype}/{name}")
}

#[tokio::test]
async fn the_authorization_matrix_over_http() {
    let (env, admin) = setup().await;
    let note = env
        .admin_insert(&admin, "note", json!({"title": "n"}))
        .await;
    let reader = env.api_token("reader").await;
    let writer = env.api_token("writer").await;
    let plain = env.api_token("plain").await;
    let one = uri("note", &note);

    // Reader: read yes, everything else 403.
    assert_eq!(
        env.send(get("/api/v1/resource/note").token(&reader))
            .await
            .status,
        StatusCode::OK
    );
    assert_eq!(
        env.send(get(&one).token(&reader)).await.status,
        StatusCode::OK
    );
    let denied = [
        post("/api/v1/resource/note")
            .token(&reader)
            .json(json!({"title": "x"})),
        req(Method::PATCH, &one)
            .token(&reader)
            .json(json!({"version": 1, "data": {"body": "x"}})),
        req(Method::DELETE, &format!("{one}?version=1")).token(&reader),
    ];
    for request in denied {
        let reply = env.send(request).await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN, "{}", reply.text);
        assert_eq!(reply.code(), "permission_denied");
    }

    // Writer: create, change and delete.
    let created = env
        .send(
            post("/api/v1/resource/note")
                .token(&writer)
                .json(json!({"title": "w"})),
        )
        .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.text);
    let mine = created.data()["name"].as_str().expect("name").to_owned();
    let patched = env
        .send(
            req(Method::PATCH, &uri("note", &mine))
                .token(&writer)
                .json(json!({"version": 1, "data": {"body": "hello", "points": 3}})),
        )
        .await;
    assert_eq!(patched.status, StatusCode::OK, "{}", patched.text);
    assert_eq!(patched.data()["version"], 2);
    assert_eq!(patched.data()["body"], "hello");
    let deleted = env
        .send(req(Method::DELETE, &format!("{}?version=2", uri("note", &mine))).token(&writer))
        .await;
    assert_eq!(deleted.status, StatusCode::NO_CONTENT);
    let gone = env.send(get(&uri("note", &mine)).token(&writer)).await;
    assert_eq!(gone.status, StatusCode::NOT_FOUND);
    assert_eq!(gone.code(), "not_found");

    // A user with no role at all, and a DocType without rules: 403 (Administrator excepted).
    for token in [&plain] {
        let reply = env.send(get("/api/v1/resource/note").token(token)).await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN);
        assert_eq!(reply.code(), "permission_denied");
    }
    for token in [&reader, &writer] {
        let reply = env.send(get("/api/v1/resource/secret").token(token)).await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN);
        let reply = env
            .send(
                post("/api/v1/resource/secret")
                    .token(token)
                    .json(json!({"x": "y"})),
            )
            .await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN);
    }
    let secret = env.admin_insert(&admin, "secret", json!({"x": "y"})).await;
    let reply = env.send(get(&uri("secret", &secret)).session(&admin)).await;
    assert_eq!(reply.status, StatusCode::OK);
    // Without a read right a get is 403, not a 404 that would hint at the name.
    let reply = env.send(get(&uri("secret", &secret)).token(&writer)).await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    // A body or a filter that does not fit the DocType never gets a 422/400 that would describe
    // a DocType the caller may not use: the missing right is reported first.
    let reply = env
        .send(
            post("/api/v1/resource/secret")
                .token(&writer)
                .json(json!({"nope": 5, "x": 1})),
        )
        .await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    let reply = env
        .send(
            req(Method::PATCH, &uri("secret", &secret))
                .token(&writer)
                .json(json!({"version": 1, "data": {"x": 5}})),
        )
        .await;
    // `x` is a Data field, so without the pre-check this would be a 422 naming it.
    assert_eq!(reply.status, StatusCode::FORBIDDEN, "{}", reply.text);
    assert_eq!(reply.code(), "permission_denied");
    // Write without Read is refused too (an update returns the document, so it needs Read):
    // the pre-check asks for both rights.
    env.user("dropper", &["Dropper"]).await;
    let dropper = env.api_token("dropper").await;
    let drop = env.admin_insert(&admin, "dropbox", json!({"x": "a"})).await;
    let reply = env
        .send(
            req(Method::PATCH, &uri("dropbox", &drop))
                .token(&dropper)
                .json(json!({"version": 1, "data": {"x": 5}})),
        )
        .await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN, "{}", reply.text);
    assert_eq!(reply.code(), "permission_denied");
    let reply = env
        .send(
            get("/api/v1/resource/secret?filters=%5B%7B%22field%22%3A%22nope%22%7D%5D")
                .token(&writer),
        )
        .await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn if_owner_hides_the_documents_of_others() {
    let (env, admin) = setup().await;
    env.user("own1", &["Own"]).await;
    env.user("own2", &["Own"]).await;
    let (own1, own2) = (env.api_token("own1").await, env.api_token("own2").await);
    let create = |token: String, title: &str| {
        let env = &env;
        let title = title.to_owned();
        async move {
            let reply = env
                .send(
                    post("/api/v1/resource/note")
                        .token(&token)
                        .json(json!({"title": title})),
                )
                .await;
            assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.text);
            reply.data()["name"].as_str().expect("name").to_owned()
        }
    };
    let n1 = create(own1.clone(), "one").await;
    let n2 = create(own2.clone(), "two").await;
    // Each sees only their own, in lists and by name.
    let list = env.send(get("/api/v1/resource/note").token(&own1)).await;
    let names: Vec<_> = list
        .data()
        .as_array()
        .expect("list")
        .iter()
        .map(|d| d["name"].clone())
        .collect();
    assert_eq!(names, [json!(n1)]);
    // Neither a filter nor a sort reveals the other's document.
    let filtered = env
        .send(get(&format!(
            "/api/v1/resource/note?filters={}",
            "%5B%7B%22field%22%3A%22title%22%2C%22op%22%3A%22%3D%22%2C%22value%22%3A%22two%22%7D%5D"
        )).token(&own1))
        .await;
    assert_eq!(filtered.data(), json!([]));
    // 404 for get, patch and delete of another's document: the same as a missing one.
    let missing = uri("note", "does-not-exist");
    let theirs = uri("note", &n2);
    for (method, target, body) in [
        (Method::GET, theirs.clone(), None),
        (Method::GET, missing.clone(), None),
        (
            Method::PATCH,
            theirs.clone(),
            Some(json!({"version": 1, "data": {"body": "x"}})),
        ),
        (
            Method::PATCH,
            missing.clone(),
            Some(json!({"version": 1, "data": {"body": "x"}})),
        ),
        (Method::DELETE, format!("{theirs}?version=1"), None),
        (Method::DELETE, format!("{missing}?version=1"), None),
    ] {
        let mut request = req(method.clone(), &target).token(&own1);
        if let Some(body) = body {
            request = request.json(body);
        }
        let reply = env.send(request).await;
        assert_eq!(
            reply.status,
            StatusCode::NOT_FOUND,
            "{method} {target}: {}",
            reply.text
        );
        assert_eq!(reply.code(), "not_found");
    }
    // The other's document is untouched; the admin sees both.
    let list = env.send(get("/api/v1/resource/note").session(&admin)).await;
    assert_eq!(list.data().as_array().expect("list").len(), 2);
    let reply = env.send(get(&theirs).token(&own2)).await;
    assert_eq!(reply.data()["version"], 1);
}

#[tokio::test]
async fn the_lifecycle_and_its_error_codes() {
    let (env, _admin) = setup().await;
    env.user("biller", &["Biller"]).await;
    env.user("viewer", &["Viewer"]).await;
    let biller = env.api_token("biller").await;
    let viewer = env.api_token("viewer").await;
    let created = env
        .send(
            post("/api/v1/resource/bill")
                .token(&biller)
                .json(json!({"title": "b"})),
        )
        .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.text);
    let name = created.data()["name"].as_str().expect("name").to_owned();
    let one = uri("bill", &name);
    assert_eq!(created.data()["docstatus"], 0);

    // Validation: every problem, in details.
    let invalid = env
        .send(
            post("/api/v1/resource/bill")
                .token(&biller)
                .json(json!({"nope": 1})),
        )
        .await;
    assert_eq!(invalid.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(invalid.code(), "validation_failed");
    assert_eq!(
        invalid.json()["error"]["details"]
            .as_array()
            .expect("details")
            .len(),
        2
    );
    // Mapping errors name fields: a caller without the right gets the 403 instead.
    let invalid = env
        .send(
            post("/api/v1/resource/note")
                .token(&biller)
                .json(json!({"title": 5})),
        )
        .await;
    assert_eq!(invalid.status, StatusCode::FORBIDDEN);

    // Version conflict.
    let stale = env
        .send(
            req(Method::PATCH, &one)
                .token(&biller)
                .json(json!({"version": 9, "data": {"title": "x"}})),
        )
        .await;
    assert_eq!(stale.status, StatusCode::CONFLICT);
    assert_eq!(stale.code(), "conflict");
    assert!(
        stale.json()["error"]["message"]
            .as_str()
            .expect("message")
            .contains("current version 1")
    );
    // Empty change.
    let empty = env
        .send(
            req(Method::PATCH, &one)
                .token(&biller)
                .json(json!({"version": 1, "data": {}})),
        )
        .await;
    assert_eq!(empty.status, StatusCode::BAD_REQUEST);

    // A viewer cannot submit; the biller can.
    let denied = env
        .send(
            post(&format!("{one}/submit"))
                .token(&viewer)
                .json(json!({"version": 1})),
        )
        .await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN);
    let submitted = env
        .send(
            post(&format!("{one}/submit"))
                .token(&biller)
                .json(json!({"version": 1})),
        )
        .await;
    assert_eq!(submitted.status, StatusCode::OK, "{}", submitted.text);
    assert_eq!(submitted.data()["docstatus"], 1);
    // A submitted document: cannot be deleted, its fields are locked, it cannot be submitted again.
    for (reply, code) in [
        (
            env.send(req(Method::DELETE, &format!("{one}?version=2")).token(&biller))
                .await,
            "invalid_docstatus",
        ),
        (
            env.send(
                req(Method::PATCH, &one)
                    .token(&biller)
                    .json(json!({"version": 2, "data": {"title": "x"}})),
            )
            .await,
            "field_locked",
        ),
        (
            env.send(
                post(&format!("{one}/submit"))
                    .token(&biller)
                    .json(json!({"version": 2})),
            )
            .await,
            "invalid_docstatus",
        ),
    ] {
        assert_eq!(reply.status, StatusCode::CONFLICT, "{}", reply.text);
        assert_eq!(reply.code(), code);
    }
    let cancelled = env
        .send(
            post(&format!("{one}/cancel"))
                .token(&biller)
                .json(json!({"version": 2})),
        )
        .await;
    assert_eq!(cancelled.status, StatusCode::OK, "{}", cancelled.text);
    assert_eq!(cancelled.data()["docstatus"], 2);
    let amended = env
        .send(
            post(&format!("{one}/amend"))
                .token(&biller)
                .json(json!({"version": 3})),
        )
        .await;
    assert_eq!(amended.status, StatusCode::CREATED, "{}", amended.text);
    assert_eq!(amended.data()["amended_from"], json!(name));
    assert_eq!(amended.data()["docstatus"], 0);
    let again = env
        .send(
            post(&format!("{one}/amend"))
                .token(&biller)
                .json(json!({"version": 3})),
        )
        .await;
    assert_eq!(again.status, StatusCode::CONFLICT);
    assert_eq!(again.code(), "already_amended");

    // Not submittable.
    env.user("writer2", &["Writer"]).await;
    let writer = env.api_token("writer2").await;
    let note = env
        .send(
            post("/api/v1/resource/note")
                .token(&writer)
                .json(json!({"title": "n"})),
        )
        .await;
    let note_name = note.data()["name"].as_str().expect("name").to_owned();
    let reply = env
        .send(
            post(&format!("{}/submit", uri("note", &note_name)))
                .token(&writer)
                .json(json!({"version": 1})),
        )
        .await;
    // `note` is not submittable: that is checked before the missing right.
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(reply.code(), "not_submittable");
}

#[tokio::test]
async fn bodies_and_parameters_are_checked() {
    let (env, admin) = setup().await;
    let note = env
        .admin_insert(&admin, "note", json!({"title": "n"}))
        .await;
    let one = uri("note", &note);
    let cases = [
        // PATCH without version, with a string version, with unknown keys, without data.
        (
            req(Method::PATCH, &one)
                .session(&admin)
                .json(json!({"data": {"body": "x"}})),
            "bad_request",
        ),
        (
            req(Method::PATCH, &one)
                .session(&admin)
                .json(json!({"version": "1", "data": {"body": "x"}})),
            "bad_request",
        ),
        (
            req(Method::PATCH, &one)
                .session(&admin)
                .json(json!({"version": 1, "data": {"body": "x"}, "x": 1})),
            "bad_request",
        ),
        (
            req(Method::PATCH, &one)
                .session(&admin)
                .json(json!({"version": 1})),
            "bad_request",
        ),
        (
            req(Method::PATCH, &one)
                .session(&admin)
                .json(json!({"version": 1, "data": []})),
            "bad_request",
        ),
        (
            post(&format!("{one}/submit"))
                .session(&admin)
                .json(json!({})),
            "bad_request",
        ),
        (
            post("/api/v1/resource/note")
                .session(&admin)
                .json(json!([1])),
            "bad_request",
        ),
        // A value that does not fit its field.
        (
            post("/api/v1/resource/note")
                .session(&admin)
                .json(json!({"title": 5})),
            "validation_failed",
        ),
        // DELETE needs `version`.
        (req(Method::DELETE, &one).session(&admin), "invalid_query"),
        (
            req(Method::DELETE, &format!("{one}?version=x")).session(&admin),
            "invalid_query",
        ),
        (
            req(Method::DELETE, &format!("{one}?version=1&x=1")).session(&admin),
            "invalid_query",
        ),
        // Query parameters.
        (
            get("/api/v1/resource/note?limit=0").session(&admin),
            "invalid_query",
        ),
        (
            get("/api/v1/resource/note?limit=501").session(&admin),
            "invalid_query",
        ),
        (
            get("/api/v1/resource/note?limit=-1").session(&admin),
            "invalid_query",
        ),
        (
            get("/api/v1/resource/note?offset=-1").session(&admin),
            "invalid_query",
        ),
        (
            get("/api/v1/resource/note?bogus=1").session(&admin),
            "invalid_query",
        ),
        (
            get("/api/v1/resource/note?sort=nope").session(&admin),
            "invalid_query",
        ),
        (
            get("/api/v1/resource/note?sort=title:sideways").session(&admin),
            "invalid_query",
        ),
        (
            get("/api/v1/resource/note?filters=%FF").session(&admin),
            "invalid_query",
        ),
        (
            get("/api/v1/resource/note?filters=not-json").session(&admin),
            "invalid_query",
        ),
        (
            get("/api/v1/resource/note?filters=%7B%7D").session(&admin),
            "invalid_query",
        ),
    ];
    for (request, code) in cases {
        let reply = env.send(request).await;
        assert!(reply.status.is_client_error(), "{}", reply.text);
        assert_eq!(reply.code(), code, "{}", reply.text);
    }
}

#[tokio::test]
async fn repeated_and_unknown_query_parameters_are_refused_everywhere() {
    let (env, admin) = setup().await;
    let note = env
        .admin_insert(&admin, "note", json!({"title": "n"}))
        .await;
    let one = uri("note", &note);
    let list = "/api/v1/resource/note";
    let cases = [
        // A repeated parameter is refused, whichever value a parser would keep.
        get(&format!("{list}?limit=1&limit=500")),
        get(&format!("{list}?limit=500&limit=1")),
        get(&format!("{list}?sort=title&sort=points")),
        req(Method::DELETE, &format!("{one}?version=1&version=2")),
        // Routes without parameters refuse any, for consistency.
        get(&format!("{one}?x=1")),
        post(&format!("{list}?x=1")).json(json!({"title": "t"})),
        req(Method::PATCH, &format!("{one}?x=1")).json(json!({"version": 1, "data": {}})),
        post(&format!("{one}/submit?x=1")).json(json!({"version": 1})),
        post(&format!("{one}/cancel?x=1")).json(json!({"version": 1})),
        post(&format!("{one}/amend?x=1")).json(json!({"version": 1})),
    ];
    for request in cases {
        let reply = env.send(request.session(&admin)).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{}", reply.text);
        assert_eq!(reply.code(), "invalid_query", "{}", reply.text);
    }
    // Nothing was created, and a single parameter still works.
    let reply = env
        .send(get(&format!("{list}?limit=1")).session(&admin))
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    assert_eq!(reply.data().as_array().expect("list").len(), 1);
    let reply = env
        .send(req(Method::DELETE, &format!("{one}?version=1")).session(&admin))
        .await;
    assert_eq!(reply.status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn listing_filters_sorts_and_pages() {
    let (env, admin) = setup().await;
    for (title, points) in [("a", 3), ("b", 1), ("c", 2)] {
        env.admin_insert(&admin, "note", json!({"title": title, "points": points}))
            .await;
    }
    let titles = |reply: common::Reply| -> Vec<String> {
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
        reply
            .data()
            .as_array()
            .expect("list")
            .iter()
            .map(|d| d["title"].as_str().expect("title").to_owned())
            .collect()
    };
    let list = |query: &str| {
        let env = &env;
        let admin = &admin;
        let uri = format!("/api/v1/resource/note?{query}");
        async move { titles(env.send(get(&uri).cookie(admin)).await) }
    };
    assert_eq!(list("sort=points").await, ["b", "c", "a"]);
    assert_eq!(list("sort=points:desc").await, ["a", "c", "b"]);
    assert_eq!(list("sort=points:desc&limit=1&offset=1").await, ["c"]);
    let filters = r#"[{"field":"points","op":">=","value":2}]"#;
    let encoded: String = filters.bytes().map(|b| format!("%{b:02X}")).collect();
    assert_eq!(
        list(&format!("filters={encoded}&sort=title")).await,
        ["a", "c"]
    );
}

#[tokio::test]
async fn decision_9_no_leaks_through_links() {
    let (env, admin) = setup().await;
    env.admin_insert(&admin, "vault", json!({"code": "v1"}))
        .await;
    env.admin_insert(&admin, "item", json!({"code": "i0"}))
        .await;
    env.admin_insert(&admin, "item", json!({"code": "i1"}))
        .await;
    let clerk = env.api_token("clerk").await;

    // A Link to a DocType the clerk cannot read: the same 422 as a Link to nothing.
    let hidden = env
        .send(
            post("/api/v1/resource/holder")
                .token(&clerk)
                .json(json!({"title": "t", "vault": "v1"})),
        )
        .await;
    let missing = env
        .send(
            post("/api/v1/resource/holder")
                .token(&clerk)
                .json(json!({"title": "t", "vault": "v2"})),
        )
        .await;
    assert_eq!(
        hidden.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "{}",
        hidden.text
    );
    assert_eq!(hidden.code(), "validation_failed");
    assert_eq!(hidden.status, missing.status);
    assert_eq!(hidden.text.replace("v1", "v2"), missing.text);
    // The admin can link to the vault.
    let linked = env
        .send(
            post("/api/v1/resource/holder")
                .session(&admin)
                .json(json!({"title": "t", "vault": "v1", "item": "i0"})),
        )
        .await;
    assert_eq!(linked.status, StatusCode::CREATED, "{}", linked.text);

    // A document that unreadable documents link to: the clerk's delete fails without saying who.
    env.admin_insert(&admin, "locked", json!({"item": "i1"}))
        .await;
    let blocked = env
        .send(req(Method::DELETE, "/api/v1/resource/item/i1?version=1").token(&clerk))
        .await;
    assert_eq!(blocked.status, StatusCode::CONFLICT, "{}", blocked.text);
    assert_eq!(blocked.code(), "linked_document_exists");
    assert!(!blocked.text.contains("locked"), "{}", blocked.text);
    assert!(!blocked.text.contains("holder"), "{}", blocked.text);
}

#[tokio::test]
async fn injection_attempts_are_inert() {
    let (env, admin) = setup().await;
    let note = env
        .admin_insert(&admin, "note", json!({"title": "n"}))
        .await;
    let evil = [
        "x%22%3B%20DROP%20TABLE%20fb_user%3B--",
        "note%22%20OR%20%221%22%3D%221",
        "..%2F..%2Fetc",
        "%27%3B%20DROP%20TABLE%20fb_user%3B--",
        "note%00",
        "fb_user",
        "doc_note",
        "fb_session",
    ];
    for name in evil {
        for request in [
            get(&format!("/api/v1/resource/{name}")),
            get(&format!("/api/v1/resource/{name}/x")),
            req(
                Method::DELETE,
                &format!("/api/v1/resource/{name}/x?version=1"),
            ),
            post(&format!("/api/v1/resource/{name}")).json(json!({})),
            post(&format!("/api/v1/resource/{name}/x/submit")).json(json!({"version": 1})),
        ] {
            let reply = env.send(request.session(&admin)).await;
            assert_eq!(
                reply.status,
                StatusCode::NOT_FOUND,
                "{name}: {}",
                reply.text
            );
            assert_eq!(reply.code(), "not_found");
        }
    }
    // Malicious filter and sort keys never reach SQL.
    for filters in [
        r#"[{"field":"title\" = 'x' OR 1=1; --","op":"=","value":"x"}]"#,
        r#"[{"field":"title","op":"= 1; DROP TABLE fb_user; --","value":"x"}]"#,
        r#"[{"field":"1=1","op":"is null"}]"#,
        r#"[{"field":"title","op":"=","value":"x","extra":"y"}]"#,
    ] {
        let reply = env
            .send(get(&format!("/api/v1/resource/note?filters={}", enc(filters))).session(&admin))
            .await;
        assert_eq!(
            reply.status,
            StatusCode::BAD_REQUEST,
            "{filters}: {}",
            reply.text
        );
    }
    for sort in [
        "title;DROP TABLE fb_user",
        "title\" desc, (select 1)",
        "(select 1):desc",
    ] {
        let reply = env
            .send(get(&format!("/api/v1/resource/note?sort={}", enc(sort))).session(&admin))
            .await;
        assert_eq!(
            reply.status,
            StatusCode::BAD_REQUEST,
            "{sort}: {}",
            reply.text
        );
    }
    // Values are bound parameters: inert text. Also as a document name and as data.
    let value = "x'; DROP TABLE fb_user; --";
    let filters = json!([{"field": "title", "op": "=", "value": value}]).to_string();
    let reply = env
        .send(get(&format!("/api/v1/resource/note?filters={}", enc(&filters))).session(&admin))
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.data(), json!([]));
    let created = env
        .admin_insert(&admin, "note", json!({"title": value}))
        .await;
    let reply = env.send(get(&uri("note", &created)).session(&admin)).await;
    assert_eq!(reply.data()["title"], value);
    let reply = env
        .send(get(&uri("note", &enc("'; DROP TABLE fb_user; --"))).session(&admin))
        .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    let reply = env.send(get(&uri("note", &note)).session(&admin)).await;
    assert_eq!(reply.status, StatusCode::OK);
    // The tables are all still there (the session above proves fb_user and fb_session).
    let list: Json = env
        .send(get("/api/v1/resource/note").session(&admin))
        .await
        .data();
    assert_eq!(list.as_array().expect("list").len(), 2);
}

#[tokio::test]
async fn a_child_doctype_is_not_a_resource_and_table_rows_round_trip() {
    let (env, admin) = setup().await;
    for name in ["note_line", "Note", "NOTE", "notes", ""] {
        let reply = env
            .send(get(&format!("/api/v1/resource/{name}")).session(&admin))
            .await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND, "{name:?}");
    }
    let name = env
        .admin_insert(
            &admin,
            "note",
            json!({"title": "t", "lines": [{"text": "a"}, {"text": "b"}]}),
        )
        .await;
    let reply = env.send(get(&uri("note", &name)).session(&admin)).await;
    let texts: Vec<_> = reply.data()["lines"]
        .as_array()
        .expect("rows")
        .iter()
        .map(|r| r["text"].clone())
        .collect();
    assert_eq!(texts, [json!("a"), json!("b")]);
}
