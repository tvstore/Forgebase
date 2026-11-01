//! The metadata routes and the session route of the desk (M4.1), against a real PostgreSQL.

mod common;

use axum::http::{Method, StatusCode};
use common::{Env, get, post, req};
use serde_json::{Value as Json, json};

async fn env_with_users() -> Env {
    let env = Env::new().await;
    for (name, roles) in [
        ("root", &["Administrator"][..]),
        ("reader", &["Reader"]),
        ("clerk", &["Clerk"]),
        ("biller", &["Biller"]),
        ("viewer", &["Viewer"]),
        ("own", &["Own"]),
        ("dropper", &["Dropper"]),
        ("nobody", &[]),
    ] {
        env.user(name, roles).await;
    }
    env
}

fn names(data: &Json) -> Vec<&str> {
    data.as_array()
        .expect("array")
        .iter()
        .map(|d| d["name"].as_str().expect("name"))
        .collect()
}

fn entry<'a>(data: &'a Json, name: &str) -> &'a Json {
    data.as_array()
        .expect("array")
        .iter()
        .find(|d| d["name"] == name)
        .unwrap_or_else(|| panic!("{name} not listed"))
}

#[tokio::test]
async fn the_meta_list_has_the_readable_doctypes_only_sorted_by_name() {
    let env = env_with_users().await;
    let expectations: [(&str, &[&str]); 7] = [
        // Administrator reads everything that is not a child DocType.
        (
            "root",
            &[
                "bill", "dropbox", "holder", "item", "ledger", "locked", "note", "secret", "vault",
            ],
        ),
        ("reader", &["holder", "item", "note"]),
        ("clerk", &["holder", "item"]),
        ("biller", &["bill"]),
        // `Dropper` may write and create `dropbox` but not read it.
        ("dropper", &[]),
        ("own", &["note"]),
        ("nobody", &[]),
    ];
    for (user, expected) in expectations {
        let session = env.login(user).await;
        let reply = env.send(get("/api/v1/meta").session(&session)).await;
        assert_eq!(reply.status, StatusCode::OK, "{user}: {}", reply.text);
        assert_eq!(names(&reply.data()), expected, "{user}");
        // The shape, and nothing about the permission rules or the roles.
        for item in reply.data().as_array().expect("array") {
            let mut keys: Vec<_> = item.as_object().expect("object").keys().collect();
            keys.sort();
            assert_eq!(
                keys,
                ["app", "icon", "label", "name", "rights", "submittable"]
            );
        }
        for role in ["Reader", "Clerk", "Writer", "Biller", "Own"] {
            if user == "root" {
                assert!(
                    !reply.text.contains(&format!("\"{role}\"")),
                    "{}",
                    reply.text
                );
            }
        }
        assert_eq!(reply.header("cache-control").as_deref(), Some("no-store"));
    }
}

#[tokio::test]
async fn rights_are_all_own_or_none_from_the_one_evaluator() {
    let env = env_with_users().await;
    let rights = |data: &Json, name: &str| entry(data, name)["rights"].clone();

    // Administrator: everything, but the lifecycle rights only where the DocType has one.
    let root = env.login("root").await;
    let data = env.send(get("/api/v1/meta").session(&root)).await.data();
    assert_eq!(
        rights(&data, "bill"),
        json!({"read": "all", "write": "all", "create": "all", "delete": "all",
               "submit": "all", "cancel": "all", "amend": "all",
               "export": "all", "import": "all"})
    );
    assert_eq!(
        rights(&data, "note"),
        json!({"read": "all", "write": "all", "create": "all", "delete": "all",
               "submit": "none", "cancel": "none", "amend": "none",
               "export": "all", "import": "all"})
    );
    assert_eq!(entry(&data, "bill")["submittable"], true);
    assert_eq!(entry(&data, "bill")["label"], "Bill");
    assert_eq!(entry(&data, "note")["submittable"], false);

    // `if_owner` rules are "own"; a right the role lacks is "none".
    let own = env.login("own").await;
    let data = env.send(get("/api/v1/meta").session(&own)).await.data();
    assert_eq!(
        rights(&data, "note"),
        json!({"read": "own", "write": "own", "create": "own", "delete": "own",
               "submit": "none", "cancel": "none", "amend": "none",
               "export": "none", "import": "none"})
    );

    let clerk = env.login("clerk").await;
    let data = env.send(get("/api/v1/meta").session(&clerk)).await.data();
    assert_eq!(
        rights(&data, "item"),
        json!({"read": "all", "write": "none", "create": "all", "delete": "all",
               "submit": "none", "cancel": "none", "amend": "none",
               "export": "none", "import": "none"})
    );

    let reader = env.login("reader").await;
    let data = env.send(get("/api/v1/meta").session(&reader)).await.data();
    assert_eq!(rights(&data, "item")["create"], "none");

    // A lifecycle right is granted per role on a submittable DocType.
    let viewer = env.login("viewer").await;
    let data = env.send(get("/api/v1/meta").session(&viewer)).await.data();
    assert_eq!(
        rights(&data, "bill"),
        json!({"read": "all", "write": "none", "create": "none", "delete": "none",
               "submit": "none", "cancel": "none", "amend": "none",
               "export": "none", "import": "none"})
    );
}

#[tokio::test]
async fn the_meta_of_one_doctype_describes_its_fields_and_child_tables() {
    let env = env_with_users().await;
    let reader = env.login("reader").await;
    let reply = env.send(get("/api/v1/meta/note").session(&reader)).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    assert_eq!(reply.header("cache-control").as_deref(), Some("no-store"));
    let data = reply.data();
    assert_eq!(data["name"], "note");
    assert_eq!(data["label"], "Note");
    assert_eq!(data["naming"], "random");
    assert_eq!(data["submittable"], false);
    assert_eq!(data["rights"]["read"], "all");
    assert_eq!(data["rights"]["write"], "none");
    assert_eq!(
        data["fields"],
        json!([
            {"name": "title", "label": "Title", "type": "data", "required": true,
             "unique": false, "allow_on_submit": false, "read_only": false, "max_length": 140},
            {"name": "body", "label": "Body", "type": "text", "required": false,
             "unique": false, "allow_on_submit": false, "read_only": false},
            {"name": "points", "label": "Points", "type": "int", "required": false,
             "unique": false, "allow_on_submit": false, "read_only": false},
            {"name": "lines", "label": "Lines", "type": "table", "required": false,
             "unique": false, "allow_on_submit": false, "read_only": false,
             "target": "note_line",
             "fields": [
                {"name": "text", "label": "Text", "type": "data", "required": false,
                 "unique": false, "allow_on_submit": false, "read_only": false,
                 "max_length": 140}
             ]}
        ])
    );
    // No `title_field` and `list_fields` in the DocType: null.
    assert_eq!(data["title_field"], Json::Null);
    assert_eq!(data["list_fields"], Json::Null);
    // Metadata only: no rules, no roles, no documents.
    assert!(!reply.text.contains("permissions"));
    assert!(!reply.text.contains("Writer"));

    let clerk = env.login("clerk").await;
    let data = env
        .send(get("/api/v1/meta/item").session(&clerk))
        .await
        .data();
    assert_eq!(data["naming"], "field:code");
    let data = env
        .send(get("/api/v1/meta/holder").session(&clerk))
        .await
        .data();
    let link = &data["fields"][1];
    assert_eq!(
        (link["name"].clone(), link["type"].clone()),
        (json!("item"), json!("link"))
    );
    assert_eq!(link["target"], "item");
}

#[tokio::test]
async fn the_meta_of_a_doctype_follows_the_same_read_rule_as_the_resource_routes() {
    let env = env_with_users().await;
    let reader = env.login("reader").await;
    // Unknown and child DocTypes: 404, also for a hostile name.
    for path in [
        "/api/v1/meta/nope",
        "/api/v1/meta/note_line",
        "/api/v1/meta/..%2fnote",
        "/api/v1/meta/note%27%3B--",
    ] {
        let reply = env.send(get(path).session(&reader)).await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND, "{path}");
        assert_eq!(reply.code(), "not_found");
    }
    // A DocType the caller cannot read: 403, DocType level, like `GET /resource`.
    for (user, doctype) in [
        ("reader", "secret"),
        ("reader", "vault"),
        ("dropper", "dropbox"),
        ("nobody", "note"),
        ("biller", "note"),
    ] {
        let session = env.login(user).await;
        let reply = env
            .send(get(&format!("/api/v1/meta/{doctype}")).session(&session))
            .await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN, "{user} {doctype}");
        assert_eq!(reply.code(), "permission_denied");
        let list = env
            .send(get(&format!("/api/v1/resource/{doctype}")).session(&session))
            .await;
        assert_eq!(list.status, StatusCode::FORBIDDEN);
        assert!(!reply.text.contains("title"), "{}", reply.text);
    }
    // Administrator reads a DocType that has no rules.
    let root = env.login("root").await;
    let reply = env.send(get("/api/v1/meta/secret").session(&root)).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.data()["fields"][0]["name"], "x");
}

#[tokio::test]
async fn meta_never_contains_data() {
    let env = env_with_users().await;
    let root = env.login("root").await;
    env.admin_insert(
        &root,
        "note",
        json!({"title": "Quarterly plan", "points": 7}),
    )
    .await;
    let reader = env.login("reader").await;
    for path in ["/api/v1/meta", "/api/v1/meta/note"] {
        let reply = env.send(get(path).session(&reader)).await;
        assert_eq!(reply.status, StatusCode::OK);
        assert!(!reply.text.contains("Quarterly"), "{path}: {}", reply.text);
    }
}

#[tokio::test]
async fn meta_needs_authentication_and_a_valid_host_and_takes_get_only() {
    let env = env_with_users().await;
    let root = env.login("root").await;
    for path in ["/api/v1/meta", "/api/v1/meta/note", "/api/v1/session"] {
        let reply = env.send(get(path)).await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{path}");
        assert_eq!(reply.code(), "unauthenticated");
        let reply = env.send(get(path).cookie_value("garbage")).await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{path}");
        for host in [None, Some("evil.example")] {
            let reply = env.send(get(path).session(&root).host(host)).await;
            assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{path}");
            assert_eq!(reply.code(), "invalid_host");
        }
        for method in [Method::POST, Method::PUT, Method::DELETE] {
            let reply = env.send(req(method, path).session(&root)).await;
            assert_eq!(reply.status, StatusCode::METHOD_NOT_ALLOWED, "{path}");
        }
    }
    // An API token works for the meta routes (GET: no CSRF).
    let token = env.api_token("reader").await;
    let reply = env.send(get("/api/v1/meta").token(&token)).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(names(&reply.data()), ["holder", "item", "note"]);
}

/// `GET /session` as the desk sends it: with the header that marks a deliberate refresh.
fn refresh(uri: &str) -> common::Req {
    get(uri).header("X-Forgebase-CSRF", "refresh")
}

#[tokio::test]
async fn the_session_route_needs_the_refresh_header_and_does_not_rotate_without_it() {
    let env = env_with_users().await;
    let login = env.login("clerk").await;
    // Without the header, or with another value: 400 and nothing is rotated.
    for request in [
        get("/api/v1/session"),
        get("/api/v1/session").header("X-Forgebase-CSRF", ""),
        get("/api/v1/session").header("X-Forgebase-CSRF", "Refresh"),
        get("/api/v1/session").header("X-Forgebase-CSRF", &login.csrf),
    ] {
        let reply = env.send(request.cookie(&login)).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{}", reply.text);
        assert_eq!(reply.code(), "bad_request");
        assert!(!reply.text.contains("csrf_token"));
    }
    // A repeated header is refused too, even when every value is the right one.
    let twice = get("/api/v1/session")
        .header("X-Forgebase-CSRF", "refresh")
        .header("X-Forgebase-CSRF", "refresh");
    let reply = env.send(twice.cookie(&login)).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{}", reply.text);
    assert!(!reply.text.contains("csrf_token"));
    // The old token still works.
    let created = env
        .send(
            post("/api/v1/resource/item")
                .session(&login)
                .json(json!({"code": "R-1"})),
        )
        .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.text);
    // With the header it rotates, and then the old token stops working.
    let reply = env.send(refresh("/api/v1/session").cookie(&login)).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    let stale = env
        .send(
            post("/api/v1/resource/item")
                .session(&login)
                .json(json!({"code": "R-2"})),
        )
        .await;
    assert_eq!(stale.code(), "csrf_failed");
    // Not logged in: still the uniform 401, header or not.
    let reply = env.send(get("/api/v1/session")).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn the_session_route_rotates_the_csrf_token() {
    let env = env_with_users().await;
    let login = env.login("clerk").await;
    let reply = env.send(refresh("/api/v1/session").cookie(&login)).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    assert_eq!(reply.header("cache-control").as_deref(), Some("no-store"));
    assert!(reply.set_cookies().is_empty());
    let data = reply.data();
    assert_eq!(data["username"], "clerk");
    assert_eq!(data["roles"], json!(["Clerk"]));
    let fresh = data["csrf_token"].as_str().expect("csrf").to_owned();
    assert_eq!(fresh.len(), 64);
    assert_ne!(fresh, login.csrf);
    assert_eq!(data.as_object().expect("object").len(), 3);
    assert!(!reply.text.contains(&login.token));

    // The old token is refused, the new one is accepted.
    let body = json!({"code": "A-1"});
    let old = env
        .send(
            post("/api/v1/resource/item")
                .session(&login)
                .json(body.clone()),
        )
        .await;
    assert_eq!(old.status, StatusCode::FORBIDDEN);
    assert_eq!(old.code(), "csrf_failed");
    let new = env
        .send(
            post("/api/v1/resource/item")
                .cookie(&login)
                .csrf(&fresh)
                .json(body),
        )
        .await;
    assert_eq!(new.status, StatusCode::CREATED, "{}", new.text);

    // Another call replaces it again; a GET with the old token is fine (GET needs none).
    let second = env
        .send(refresh("/api/v1/session").cookie(&login))
        .await
        .data();
    assert_ne!(second["csrf_token"].as_str(), Some(fresh.as_str()));
    let stale = env
        .send(
            post("/api/v1/resource/item")
                .cookie(&login)
                .csrf(&fresh)
                .json(json!({"code": "A-2"})),
        )
        .await;
    assert_eq!(stale.code(), "csrf_failed");

    // Only a digest is stored.
    let rows = env
        .db
        .pool
        .get()
        .await
        .expect("connection")
        .query_one(
            "SELECT count(*) FROM fb_session WHERE csrf_hash = sha256(convert_to($1::text, 'UTF8'))",
            &[&second["csrf_token"].as_str().expect("csrf")],
        )
        .await
        .expect("query")
        .get::<_, i64>(0);
    assert_eq!(rows, 1);
}

#[tokio::test]
async fn the_session_route_needs_a_valid_cookie_session() {
    let env = env_with_users().await;
    // An API token has no session.
    let token = env.api_token("clerk").await;
    let reply = env.send(refresh("/api/v1/session").token(&token)).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(reply.code(), "bad_request");
    assert!(!reply.text.contains("csrf_token"));

    // Expired (absolute and idle) and logged-out sessions are the uniform 401, and nothing is
    // rotated for them.
    let absolute = env.login("clerk").await;
    env.db
        .exec("UPDATE fb_session SET expires_at = now() - interval '1 second'")
        .await;
    let reply = env.send(refresh("/api/v1/session").cookie(&absolute)).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    assert_eq!(reply.code(), "unauthenticated");

    let idle = env.login("reader").await;
    env.db
        .exec("UPDATE fb_session SET last_seen_at = now() - interval '25 hours'")
        .await;
    let reply = env.send(refresh("/api/v1/session").cookie(&idle)).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);

    let gone = env.login("biller").await;
    let out = env.send(post("/api/v1/logout").session(&gone)).await;
    assert_eq!(out.status, StatusCode::NO_CONTENT);
    let reply = env.send(refresh("/api/v1/session").cookie(&gone)).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn the_meta_shows_read_only_fields_title_field_and_list_fields() {
    let env = env_with_users().await;
    let root = env.login("root").await;
    let data = env
        .send(get("/api/v1/meta/ledger").session(&root))
        .await
        .data();
    assert_eq!(data["label"], "Ledger");
    assert_eq!(data["title_field"], "title");
    assert_eq!(data["list_fields"], json!(["title", "total"]));
    let fields = data["fields"].as_array().expect("fields");
    let read_only = |field: &Json| (field["name"].clone(), field["read_only"].clone());
    assert_eq!(
        fields.iter().map(read_only).collect::<Vec<_>>(),
        [
            (json!("title"), json!(false)),
            (json!("total"), json!(true)),
            (json!("lines"), json!(false)),
        ]
    );
    // The inlined child fields carry the flag too, and a humanized label.
    let child = fields[2]["fields"].as_array().expect("child fields");
    assert_eq!(
        child.iter().map(read_only).collect::<Vec<_>>(),
        [(json!("qty"), json!(false)), (json!("amount"), json!(true))]
    );
    assert_eq!(child[1]["label"], "Amount");
}

/// A user restricted on a DocType itself (user permissions, ADR 0025) cannot create or amend
/// documents of it, so the metadata says `none` for those two and is unchanged for the rest;
/// a DocType they are not restricted on, and a user without rows, are unchanged.
#[tokio::test]
async fn rights_of_a_restricted_user_say_none_for_create_and_amend() {
    let env = env_with_users().await;
    let root = env.login("root").await;
    let name = env
        .admin_insert(&root, "bill", json!({"title": "b1"}))
        .await;
    env.user("limited", &["Biller"]).await;
    env.db
        .exec(&format!(
            "INSERT INTO fb_user_permission (user_id, doctype, value) \
         SELECT id, 'bill', '{name}' FROM fb_user WHERE username = 'limited'"
        ))
        .await;
    let rights = |data: &Json, doctype: &str| entry(data, doctype)["rights"].clone();

    let limited = env.login("limited").await;
    let data = env.send(get("/api/v1/meta").session(&limited)).await.data();
    assert_eq!(
        rights(&data, "bill"),
        json!({"read": "all", "write": "all", "create": "none", "delete": "all",
               "submit": "all", "cancel": "all", "amend": "none",
               "export": "none", "import": "none"})
    );
    // The single-DocType route says the same.
    let one = env
        .send(get("/api/v1/meta/bill").session(&limited))
        .await
        .data();
    assert_eq!(one["rights"]["create"], "none");
    assert_eq!(one["rights"]["amend"], "none");
    assert_eq!(one["rights"]["write"], "all");

    // Unchanged for a user without rows, and for the Administrator whatever rows exist.
    let biller = env.login("biller").await;
    let data = env.send(get("/api/v1/meta").session(&biller)).await.data();
    assert_eq!(rights(&data, "bill")["create"], "all");
    assert_eq!(rights(&data, "bill")["amend"], "all");
    env.db
        .exec(
            "INSERT INTO fb_user_permission (user_id, doctype, value) \
         SELECT id, 'bill', 'x' FROM fb_user WHERE username = 'root'",
        )
        .await;
    let root = env.login("root").await;
    let data = env.send(get("/api/v1/meta").session(&root)).await.data();
    assert_eq!(rights(&data, "bill")["create"], "all");
    assert_eq!(rights(&data, "bill")["amend"], "all");
}
