//! Saved list views and the group-by summary over HTTP (M8.3): each route with its status
//! codes, authentication, CSRF, the envelopes, the 404 for another user's private view and the
//! read scope of the summary.

mod common;

use axum::http::{Method, StatusCode};
use common::{Env, Reply, Session, get, post, req};
use serde_json::{Value as Json, json};

/// `Clerk` reads and creates everything, `Mine` reads and creates only its own documents.
const DEAL: &str = r#"
name = "deal"
label = "Deal"
[[fields]]
name = "title"
type = "data"
[[fields]]
name = "stage"
type = "select"
options = ["new", "won", "lost"]
[[fields]]
name = "amount"
type = "decimal"
precision = 10
scale = 2
[[fields]]
name = "qty"
type = "int"
[[fields]]
name = "notes"
type = "text"
[[fields]]
name = "lines"
type = "table"
target = "deal_line"
[[permissions]]
role = "Clerk"
read = true
create = true
[[permissions]]
role = "Mine"
read = true
create = true
if_owner = true
"#;

const DEAL_LINE: &str = r#"
name = "deal_line"
label = "Deal Line"
is_child = true
[[fields]]
name = "qty"
type = "int"
"#;

struct Setup {
    env: Env,
    ann: Session,
    bob: Session,
    root: Session,
}

async fn setup() -> Setup {
    let env = Env::with_doctypes(&[DEAL, DEAL_LINE]).await;
    env.user("ann", &["Clerk"]).await;
    env.user("bob", &["Clerk"]).await;
    env.user("root", &["Administrator"]).await;
    env.user("nobody", &[]).await;
    let (ann, bob, root) = (
        env.login("ann").await,
        env.login("bob").await,
        env.login("root").await,
    );
    Setup {
        env,
        ann,
        bob,
        root,
    }
}

fn definition() -> Json {
    json!({
        "filters": [{"field": "stage", "op": "=", "value": "new"}],
        "sort": ["title:desc"],
        "columns": ["title", "stage"],
    })
}

async fn create(s: &Setup, who: &Session, label: &str, shared: bool) -> Reply {
    s.env
        .send(
            post("/api/v1/views/deal")
                .session(who)
                .json(json!({"label": label, "definition": definition(), "shared": shared})),
        )
        .await
}

fn status(reply: &Reply, status: StatusCode, code: &str) {
    assert_eq!(reply.status, status, "{}", reply.text);
    if !code.is_empty() {
        assert_eq!(reply.code(), code, "{}", reply.text);
    }
}

#[tokio::test]
async fn the_routes_create_list_update_and_delete_with_their_envelopes() {
    let s = setup().await;
    let created = create(&s, &s.ann, "Open deals", false).await;
    status(&created, StatusCode::CREATED, "");
    let view = created.data();
    assert_eq!(
        view.as_object().unwrap().keys().collect::<Vec<_>>(),
        [
            "definition",
            "id",
            "label",
            "modified_at",
            "owner",
            "shared",
            "version"
        ]
    );
    assert_eq!(view["label"], "Open deals");
    assert_eq!(view["shared"], false);
    assert_eq!(view["owner"], "ann");
    assert_eq!(view["version"], 1);
    assert_eq!(view["definition"], definition());
    assert!(view["modified_at"].as_str().unwrap().ends_with('Z'));
    let id = view["id"].as_str().unwrap().to_owned();

    let list = s.env.send(get("/api/v1/views/deal").session(&s.ann)).await;
    status(&list, StatusCode::OK, "");
    assert_eq!(list.json().as_object().unwrap().len(), 1);
    assert_eq!(list.data(), json!([view]));

    // Only the label; then only the definition.
    let url = format!("/api/v1/views/deal/{id}");
    let renamed = s
        .env
        .send(
            req(Method::PATCH, &url)
                .session(&s.ann)
                .json(json!({"version": 1, "label": "Won"})),
        )
        .await;
    status(&renamed, StatusCode::OK, "");
    assert_eq!(renamed.data()["label"], "Won");
    assert_eq!(renamed.data()["version"], 2);
    assert_eq!(renamed.data()["definition"], definition());
    let new = json!({"filters": [], "sort": [], "columns": []});
    let redefined = s
        .env
        .send(
            req(Method::PATCH, &url)
                .session(&s.ann)
                .json(json!({"version": 2, "definition": new})),
        )
        .await;
    assert_eq!(redefined.data()["definition"], new, "{}", redefined.text);
    // A partial definition is completed.
    let partial = s
        .env
        .send(
            req(Method::PATCH, &url)
                .session(&s.ann)
                .json(json!({"version": 3, "definition": {"columns": ["title"]}})),
        )
        .await;
    assert_eq!(
        partial.data()["definition"],
        json!({"filters": [], "sort": [], "columns": ["title"]})
    );

    // A stale version, and a delete with a stale version.
    let stale = s
        .env
        .send(
            req(Method::PATCH, &url)
                .session(&s.ann)
                .json(json!({"version": 1, "label": "x"})),
        )
        .await;
    status(&stale, StatusCode::CONFLICT, "conflict");
    status(
        &s.env
            .send(req(Method::DELETE, &format!("{url}?version=1")).session(&s.ann))
            .await,
        StatusCode::CONFLICT,
        "conflict",
    );
    let deleted = s
        .env
        .send(req(Method::DELETE, &format!("{url}?version=4")).session(&s.ann))
        .await;
    assert_eq!(deleted.status, StatusCode::NO_CONTENT);
    assert!(deleted.text.is_empty());
    status(
        &s.env
            .send(req(Method::DELETE, &format!("{url}?version=4")).session(&s.ann))
            .await,
        StatusCode::NOT_FOUND,
        "not_found",
    );
    assert_eq!(
        s.env
            .send(get("/api/v1/views/deal").session(&s.ann))
            .await
            .data(),
        json!([])
    );
}

#[tokio::test]
async fn authentication_and_csrf_are_needed_like_on_the_resource_routes() {
    let s = setup().await;
    let created = create(&s, &s.ann, "Mine", false).await;
    let id = created.data()["id"].as_str().unwrap().to_owned();
    let url = format!("/api/v1/views/deal/{id}");
    let body = json!({"label": "x", "definition": definition()});
    // 401 without a credential.
    for request in [
        get("/api/v1/views/deal"),
        post("/api/v1/views/deal").json(body.clone()),
        req(Method::PATCH, &url).json(json!({"version": 1, "label": "x"})),
        req(Method::DELETE, &format!("{url}?version=1")),
        get("/api/v1/summary/deal?group_by=stage"),
    ] {
        status(
            &s.env.send(request).await,
            StatusCode::UNAUTHORIZED,
            "unauthenticated",
        );
    }
    // 403 without the CSRF token on the writes; a read needs none.
    for request in [
        post("/api/v1/views/deal").cookie(&s.ann).json(body),
        req(Method::PATCH, &url)
            .cookie(&s.ann)
            .json(json!({"version": 1, "label": "x"})),
        req(Method::DELETE, &format!("{url}?version=1")).cookie(&s.ann),
    ] {
        status(
            &s.env.send(request).await,
            StatusCode::FORBIDDEN,
            "csrf_failed",
        );
    }
    status(
        &s.env.send(get("/api/v1/views/deal").cookie(&s.ann)).await,
        StatusCode::OK,
        "",
    );
    // An API token needs no CSRF header.
    let token = s.env.api_token("ann").await;
    status(
        &s.env.send(get("/api/v1/views/deal").token(&token)).await,
        StatusCode::OK,
        "",
    );
    status(
        &s.env
            .send(req(Method::DELETE, &format!("{url}?version=1")).token(&token))
            .await,
        StatusCode::NO_CONTENT,
        "",
    );
}

#[tokio::test]
async fn another_users_private_view_is_not_found_not_forbidden() {
    let s = setup().await;
    let id = create(&s, &s.ann, "Private", false).await.data()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let url = format!("/api/v1/views/deal/{id}");
    for who in [&s.bob, &s.root] {
        assert_eq!(
            s.env
                .send(get("/api/v1/views/deal").session(who))
                .await
                .data(),
            json!([])
        );
        let patch = s
            .env
            .send(
                req(Method::PATCH, &url)
                    .session(who)
                    .json(json!({"version": 1, "label": "x"})),
            )
            .await;
        status(&patch, StatusCode::NOT_FOUND, "not_found");
        let delete = s
            .env
            .send(req(Method::DELETE, &format!("{url}?version=1")).session(who))
            .await;
        status(&delete, StatusCode::NOT_FOUND, "not_found");
        // The message of a hidden view is the one of a missing one.
        let missing = s
            .env
            .send(
                req(
                    Method::DELETE,
                    "/api/v1/views/deal/00000000-0000-7000-8000-000000000000?version=1",
                )
                .session(who),
            )
            .await;
        let text = |r: &Reply| {
            r.json()["error"]["message"]
                .as_str()
                .unwrap()
                .replace(&id, "ID")
        };
        assert_eq!(
            text(&delete),
            text(&missing).replace("00000000-0000-7000-8000-000000000000", "ID")
        );
    }
    assert_eq!(
        s.env
            .send(get("/api/v1/views/deal").session(&s.ann))
            .await
            .data()[0]["label"],
        "Private"
    );
    // A path that is no UUID names no view.
    status(
        &s.env
            .send(req(Method::DELETE, "/api/v1/views/deal/nope?version=1").session(&s.ann))
            .await,
        StatusCode::NOT_FOUND,
        "not_found",
    );
}

#[tokio::test]
async fn shared_views_are_for_administrators_to_create_and_everyone_to_see() {
    let s = setup().await;
    status(
        &create(&s, &s.ann, "Team", true).await,
        StatusCode::FORBIDDEN,
        "permission_denied",
    );
    let shared = create(&s, &s.root, "Team", true).await;
    status(&shared, StatusCode::CREATED, "");
    assert_eq!(shared.data()["shared"], true);
    assert_eq!(shared.data()["owner"], Json::Null);
    let id = shared.data()["id"].as_str().unwrap().to_owned();
    create(&s, &s.bob, "B mine", false).await;
    let seen = s
        .env
        .send(get("/api/v1/views/deal").session(&s.bob))
        .await
        .data();
    let labels: Vec<&str> = seen
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["label"].as_str().unwrap())
        .collect();
    assert_eq!(labels, ["Team", "B mine"]);
    let url = format!("/api/v1/views/deal/{id}");
    status(
        &s.env
            .send(
                req(Method::PATCH, &url)
                    .session(&s.ann)
                    .json(json!({"version": 1, "label": "x"})),
            )
            .await,
        StatusCode::FORBIDDEN,
        "permission_denied",
    );
    status(
        &s.env
            .send(req(Method::DELETE, &format!("{url}?version=1")).session(&s.ann))
            .await,
        StatusCode::FORBIDDEN,
        "permission_denied",
    );
    status(
        &s.env
            .send(req(Method::DELETE, &format!("{url}?version=1")).session(&s.root))
            .await,
        StatusCode::NO_CONTENT,
        "",
    );
}

#[tokio::test]
async fn the_doctype_and_the_read_right_are_checked_first() {
    let s = setup().await;
    let nobody = s.env.login("nobody").await;
    status(
        &s.env.send(get("/api/v1/views/deal").session(&nobody)).await,
        StatusCode::FORBIDDEN,
        "permission_denied",
    );
    // Before the body is looked at: a bad body gives a caller without the right nothing.
    status(
        &s.env
            .send(
                post("/api/v1/views/deal")
                    .session(&nobody)
                    .json(json!({"nonsense": 1})),
            )
            .await,
        StatusCode::FORBIDDEN,
        "permission_denied",
    );
    for uri in [
        "/api/v1/views/nope",
        "/api/v1/views/deal_line",
        "/api/v1/summary/nope?group_by=x",
        "/api/v1/summary/deal_line?group_by=qty",
    ] {
        status(
            &s.env.send(get(uri).session(&s.ann)).await,
            StatusCode::NOT_FOUND,
            "not_found",
        );
    }
    status(
        &s.env
            .send(post("/api/v1/views/nope").session(&s.ann).json(json!({})))
            .await,
        StatusCode::NOT_FOUND,
        "not_found",
    );
    status(
        &s.env
            .send(get("/api/v1/summary/deal?group_by=stage").session(&nobody))
            .await,
        StatusCode::FORBIDDEN,
        "permission_denied",
    );
}

#[tokio::test]
async fn bad_bodies_and_definitions_are_refused() {
    let s = setup().await;
    let post_body = |body: Json| {
        let (env, ann) = (&s.env, &s.ann);
        async move {
            env.send(post("/api/v1/views/deal").session(ann).json(body))
                .await
        }
    };
    let with = |definition: Json| json!({"label": "v", "definition": definition});
    // 422: the label or the definition is not valid.
    for (body, needle) in [
        (json!({"label": "", "definition": definition()}), "label"),
        (
            json!({"label": " x", "definition": definition()}),
            "whitespace",
        ),
        (
            with(json!({"filters": [{"field": "nope", "op": "is null"}]})),
            "unknown field",
        ),
        (
            with(json!({"filters": [{"field": "qty", "op": "like", "value": "1%"}]})),
            "text fields only",
        ),
        (
            with(json!({"filters": [{"field": "qty", "op": "?", "value": 1}]})),
            "unknown operator",
        ),
        (with(json!({"filters": "x"})), "must be a list"),
        (with(json!({"sort": ["nope"]})), "unknown field"),
        (with(json!({"sort": ["title:sideways"]})), "invalid sort"),
        (with(json!({"sort": [1]})), "list of strings"),
        (with(json!({"columns": ["lines"]})), "table field"),
        (with(json!({"columns": ["notes"]})), "text field"),
        (with(json!({"columns": ["nope"]})), "unknown column"),
        (with(json!({"columns": ["title", "title"]})), "twice"),
        (
            with(
                json!({"columns": ["title", "stage", "amount", "qty", "name", "owner", "modified_by", "created_at", "modified_at"]}),
            ),
            "at most 8",
        ),
        (with(json!({"limit": 5})), "unknown key"),
        (with(json!("x")), "must be an object"),
    ] {
        let reply = post_body(body.clone()).await;
        status(
            &reply,
            StatusCode::UNPROCESSABLE_ENTITY,
            "validation_failed",
        );
        let details = reply.json()["error"]["details"][0]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(details.contains(needle), "{body}: {details}");
    }
    // A definition over 16 KiB.
    let big = "x".repeat(17 * 1024);
    status(
        &post_body(with(
            json!({"filters": [{"field": "title", "op": "=", "value": big}]}),
        ))
        .await,
        StatusCode::UNPROCESSABLE_ENTITY,
        "validation_failed",
    );
    // 400: the body is not what the route takes.
    for body in [
        json!({"definition": definition()}),
        json!({"label": "v"}),
        json!({"label": 5, "definition": definition()}),
        json!({"label": "v", "definition": definition(), "shared": "yes"}),
        json!({"label": "v", "definition": definition(), "owner": "bob"}),
        json!([]),
    ] {
        status(
            &post_body(body).await,
            StatusCode::BAD_REQUEST,
            "bad_request",
        );
    }
    let view = create(&s, &s.ann, "ok", false).await.data();
    let url = format!("/api/v1/views/deal/{}", view["id"].as_str().unwrap());
    for body in [
        json!({"label": "x"}),
        json!({"version": "1", "label": "x"}),
        json!({"version": 1, "label": "x", "shared": true}),
        json!({"version": 1, "label": 7}),
    ] {
        status(
            &s.env
                .send(req(Method::PATCH, &url).session(&s.ann).json(body))
                .await,
            StatusCode::BAD_REQUEST,
            "bad_request",
        );
    }
    // No change at all.
    status(
        &s.env
            .send(
                req(Method::PATCH, &url)
                    .session(&s.ann)
                    .json(json!({"version": 1})),
            )
            .await,
        StatusCode::BAD_REQUEST,
        "bad_request",
    );
    // Unknown and repeated parameters, and a missing or bad version.
    status(
        &s.env
            .send(get("/api/v1/views/deal?x=1").session(&s.ann))
            .await,
        StatusCode::BAD_REQUEST,
        "invalid_query",
    );
    for suffix in ["", "?version=x", "?version=1&version=1", "?version=1&x=1"] {
        status(
            &s.env
                .send(req(Method::DELETE, &format!("{url}{suffix}")).session(&s.ann))
                .await,
            StatusCode::BAD_REQUEST,
            "invalid_query",
        );
    }
    // Not JSON.
    let raw = s
        .env
        .send(
            post("/api/v1/views/deal")
                .session(&s.ann)
                .raw("application/json", b"{"),
        )
        .await;
    status(&raw, StatusCode::BAD_REQUEST, "bad_request");
    let text = s
        .env
        .send(
            post("/api/v1/views/deal")
                .session(&s.ann)
                .raw("text/plain", b"{}"),
        )
        .await;
    status(
        &text,
        StatusCode::UNSUPPORTED_MEDIA_TYPE,
        "unsupported_media_type",
    );
}

#[tokio::test]
async fn a_duplicate_label_is_a_conflict_and_a_stale_view_is_stored_as_written() {
    let s = setup().await;
    status(
        &create(&s, &s.ann, "Same", false).await,
        StatusCode::CREATED,
        "",
    );
    let duplicate = create(&s, &s.ann, "Same", false).await;
    status(&duplicate, StatusCode::CONFLICT, "conflict");
    assert!(
        duplicate.json()["error"]["message"]
            .as_str()
            .unwrap()
            .contains("\"Same\"")
    );
    status(
        &create(&s, &s.bob, "Same", false).await,
        StatusCode::CREATED,
        "",
    );
    // A view that names a field the DocType no longer has is listed as stored.
    s.env
        .db
        .exec(
            "INSERT INTO fb_saved_view (id, doctype, label, owner_id, definition) VALUES \
             (gen_random_uuid(), 'deal', 'Old', NULL, \
             '{\"filters\":[{\"field\":\"gone\",\"op\":\"is null\"}],\"sort\":[],\"columns\":[\"gone\"]}')",
        )
        .await;
    let list = s
        .env
        .send(get("/api/v1/views/deal").session(&s.ann))
        .await
        .data();
    assert_eq!(list[0]["label"], "Old");
    assert_eq!(list[0]["definition"]["columns"], json!(["gone"]));
    // Applying it is the list's business: invalid_query.
    let applied = s
        .env
        .send(
            get("/api/v1/resource/deal?filters=%5B%7B%22field%22%3A%22gone%22%2C%22op%22%3A%22is%20null%22%7D%5D")
                .session(&s.ann),
        )
        .await;
    status(&applied, StatusCode::BAD_REQUEST, "invalid_query");
}

async fn add_deals(s: &Setup) {
    for (who, title, stage, amount, qty) in [
        (&s.ann, "a", "new", "10.00", 1),
        (&s.ann, "b", "new", "20.50", 2),
        (&s.ann, "c", "won", "100.00", 5),
        (&s.bob, "d", "lost", "7.00", 3),
        (&s.bob, "e", "new", "1.00", 4),
    ] {
        let reply = s
            .env
            .send(
                post("/api/v1/resource/deal")
                    .session(who)
                    .json(json!({"title": title, "stage": stage, "amount": amount, "qty": qty})),
            )
            .await;
        status(&reply, StatusCode::CREATED, "");
    }
}

#[tokio::test]
async fn the_summary_groups_with_aggregates_and_the_envelope() {
    let s = setup().await;
    add_deals(&s).await;
    let reply = s
        .env
        .send(
            get("/api/v1/summary/deal?group_by=stage&aggregates=sum:amount,avg:amount,sum:qty,max:qty")
                .session(&s.ann),
        )
        .await;
    status(&reply, StatusCode::OK, "");
    assert_eq!(reply.json().as_object().unwrap().len(), 1);
    assert_eq!(
        reply.data(),
        json!({
            "groups": [
                {"key": "new", "count": 3, "values": {
                    "sum_amount": "31.50", "avg_amount": "10.5000", "sum_qty": 7, "max_qty": 4}},
                {"key": "lost", "count": 1, "values": {
                    "sum_amount": "7.00", "avg_amount": "7.0000", "sum_qty": 3, "max_qty": 3}},
                {"key": "won", "count": 1, "values": {
                    "sum_amount": "100.00", "avg_amount": "100.0000", "sum_qty": 5, "max_qty": 5}},
            ],
            "truncated": false
        })
    );
    // Without aggregates, with a filter (the list's JSON form), by owner.
    let filters =
        "%5B%7B%22field%22%3A%22stage%22%2C%22op%22%3A%22%3D%22%2C%22value%22%3A%22new%22%7D%5D";
    let reply = s
        .env
        .send(
            get(&format!(
                "/api/v1/summary/deal?group_by=owner&filters={filters}"
            ))
            .session(&s.ann),
        )
        .await;
    assert_eq!(
        reply.data(),
        json!({"groups": [
            {"key": "ann", "count": 2, "values": {}},
            {"key": "bob", "count": 1, "values": {}}
        ], "truncated": false})
    );
}

#[tokio::test]
async fn the_summary_respects_if_owner() {
    let s = setup().await;
    s.env.user("mine", &["Mine"]).await;
    let mine = s.env.login("mine").await;
    add_deals(&s).await;
    for stage in ["new", "new", "won"] {
        let reply = s
            .env
            .send(
                post("/api/v1/resource/deal")
                    .session(&mine)
                    .json(json!({"stage": stage, "qty": 1})),
            )
            .await;
        status(&reply, StatusCode::CREATED, "");
    }
    let reply = s
        .env
        .send(get("/api/v1/summary/deal?group_by=stage&aggregates=sum:qty").session(&mine))
        .await;
    assert_eq!(
        reply.data()["groups"],
        json!([
            {"key": "new", "count": 2, "values": {"sum_qty": 2}},
            {"key": "won", "count": 1, "values": {"sum_qty": 1}},
        ])
    );
    // The Clerk sees all eight.
    let all = s
        .env
        .send(get("/api/v1/summary/deal?group_by=stage").session(&s.ann))
        .await;
    let total: i64 = all.data()["groups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g["count"].as_i64().unwrap())
        .sum();
    assert_eq!(total, 8);
}

#[tokio::test]
async fn bad_summary_queries_are_invalid_queries() {
    let s = setup().await;
    for query in [
        "",
        "group_by=",
        "group_by=nope",
        "group_by=notes",
        "group_by=amount",
        "group_by=lines",
        "group_by=name",
        "group_by=stage&aggregates=sum:title",
        "group_by=stage&aggregates=sum:nope",
        "group_by=stage&aggregates=count:qty",
        "group_by=stage&aggregates=sum",
        "group_by=stage&aggregates=sum:qty,sum:qty",
        "group_by=stage&aggregates=sum:qty,avg:qty,min:qty,max:qty,sum:amount,avg:amount,min:amount",
        "group_by=stage&filters=%5B%7B%22field%22%3A%22nope%22%2C%22op%22%3A%22is%20null%22%7D%5D",
        "group_by=stage&filters=%5B",
        "group_by=stage&limit=5",
        "group_by=stage&group_by=owner",
    ] {
        let reply = s
            .env
            .send(get(&format!("/api/v1/summary/deal?{query}")).session(&s.ann))
            .await;
        assert_eq!(
            reply.status,
            StatusCode::BAD_REQUEST,
            "{query}: {}",
            reply.text
        );
        assert_eq!(reply.code(), "invalid_query", "{query}: {}", reply.text);
    }
    // The summary is a read: it takes no body method.
    status(
        &s.env
            .send(post("/api/v1/summary/deal").session(&s.ann).json(json!({})))
            .await,
        StatusCode::METHOD_NOT_ALLOWED,
        "method_not_allowed",
    );
}
