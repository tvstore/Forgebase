//! Link titles over HTTP (UX.4): `?link_titles=1` adds a sibling `link_titles` key, follows the
//! caller's read rules, and leaves the output unchanged without the parameter.

mod common;

use axum::http::StatusCode;
use common::{Env, get, registry_with, send_to};
use serde_json::json;

/// A titled target: `Reader` reads all, `Mine` only its own documents.
const CUST: &str = r#"
name = "cust"
label = "Cust"
naming = { field = "code" }
title_field = "full_name"
icon = "users"
[[fields]]
name = "code"
type = "data"
required = true
unique = true
[[fields]]
name = "full_name"
type = "data"
[[permissions]]
role = "Reader"
read = true
create = true
[[permissions]]
role = "Mine"
read = true
if_owner = true
"#;

const DEAL: &str = r#"
name = "deal"
label = "Deal"
[[fields]]
name = "cust"
type = "link"
target = "cust"
[[fields]]
name = "lines"
type = "table"
target = "deal_line"
[[permissions]]
role = "Reader"
read = true
create = true
[[permissions]]
role = "Mine"
read = true
if_owner = true
"#;

const DEAL_LINE: &str = r#"
name = "deal_line"
label = "Deal Line"
is_child = true
[[fields]]
name = "cust"
type = "link"
target = "cust"
"#;

async fn setup() -> (Env, String) {
    let env = Env::with_doctypes(&[CUST, DEAL, DEAL_LINE]).await;
    env.user("admin", &["Administrator"]).await;
    env.user("reader", &["Reader"]).await;
    env.user("mine", &["Mine"]).await;
    env.user("nobody", &[]).await;
    let admin = env.login("admin").await;
    env.admin_insert(&admin, "cust", json!({"code": "C-1", "full_name": "Ada"}))
        .await;
    env.admin_insert(&admin, "cust", json!({"code": "C-2"}))
        .await;
    env.admin_insert(
        &admin,
        "deal",
        json!({"cust": "C-1", "lines": [{"cust": "C-2"}, {"cust": "C-1"}]}),
    )
    .await;
    let reader = env.api_token("reader").await;
    (env, reader)
}

#[tokio::test]
async fn the_output_is_unchanged_without_the_parameter() {
    let (env, reader) = setup().await;
    let list = env.send(get("/api/v1/resource/deal").token(&reader)).await;
    assert_eq!(list.status, StatusCode::OK, "{}", list.text);
    assert!(list.json().get("link_titles").is_none(), "{}", list.text);
    assert_eq!(list.json().as_object().unwrap().len(), 1);
    let name = list.data()[0]["name"].as_str().unwrap().to_owned();
    let one = env
        .send(get(&format!("/api/v1/resource/deal/{name}")).token(&reader))
        .await;
    assert_eq!(one.json().as_object().unwrap().len(), 1, "{}", one.text);
}

#[tokio::test]
async fn list_and_read_carry_link_titles_when_asked() {
    let (env, reader) = setup().await;
    let list = env
        .send(get("/api/v1/resource/deal?link_titles=1").token(&reader))
        .await;
    assert_eq!(list.status, StatusCode::OK, "{}", list.text);
    // The title of C-1 only: C-2 has none. The data is as without the parameter.
    assert_eq!(list.json()["link_titles"], json!({"cust": {"C-1": "Ada"}}));
    assert_eq!(list.data()[0]["cust"], "C-1");
    let name = list.data()[0]["name"].as_str().unwrap().to_owned();
    let one = env
        .send(get(&format!("/api/v1/resource/deal/{name}?link_titles=1")).token(&reader))
        .await;
    assert_eq!(one.status, StatusCode::OK, "{}", one.text);
    assert_eq!(one.json()["link_titles"], json!({"cust": {"C-1": "Ada"}}));
    assert_eq!(one.data()["name"], name);
}

#[tokio::test]
async fn an_if_owner_caller_gets_no_title_for_someone_elses_document() {
    let (env, _) = setup().await;
    let mine = env.api_token("mine").await;
    // `mine` reads only its own documents: the deal is the admin's, so there is nothing to
    // title; and the target is the admin's too.
    let list = env
        .send(get("/api/v1/resource/deal?link_titles=1").token(&mine))
        .await;
    assert_eq!(list.status, StatusCode::OK, "{}", list.text);
    assert_eq!(list.data(), json!([]));
    assert_eq!(list.json()["link_titles"], json!({}));
    // A caller with no right on the DocType is refused as before.
    let nobody = env.api_token("nobody").await;
    let refused = env
        .send(get("/api/v1/resource/deal?link_titles=1").token(&nobody))
        .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn a_bad_value_is_invalid_query() {
    let (env, reader) = setup().await;
    for value in ["0", "true", "", "2"] {
        let reply = env
            .send(get(&format!("/api/v1/resource/deal?link_titles={value}")).token(&reader))
            .await;
        assert_eq!(
            reply.status,
            StatusCode::BAD_REQUEST,
            "{value}: {}",
            reply.text
        );
        assert_eq!(reply.code(), "invalid_query");
    }
    let list = env.send(get("/api/v1/resource/deal").token(&reader)).await;
    let name = list.data()[0]["name"].as_str().unwrap().to_owned();
    let reply = env
        .send(get(&format!("/api/v1/resource/deal/{name}?link_titles=yes")).token(&reader))
        .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(reply.code(), "invalid_query");
    // A repeated parameter is refused like every other.
    let twice = env
        .send(get("/api/v1/resource/deal?link_titles=1&link_titles=1").token(&reader))
        .await;
    assert_eq!(twice.code(), "invalid_query");
}

#[tokio::test]
async fn the_meta_carries_the_icon() {
    let (env, reader) = setup().await;
    let one = env.send(get("/api/v1/meta/cust").token(&reader)).await;
    assert_eq!(one.data()["icon"], "users");
    let one = env.send(get("/api/v1/meta/deal").token(&reader)).await;
    assert_eq!(one.data()["icon"], json!(null));
    let all = env.send(get("/api/v1/meta").token(&reader)).await;
    let by_name = |name: &str| {
        all.data()
            .as_array()
            .unwrap()
            .iter()
            .find(|d| d["name"] == name)
            .cloned()
            .unwrap()
    };
    assert_eq!(by_name("cust")["icon"], "users");
    assert_eq!(by_name("deal")["icon"], json!(null));
}

/// With a pool of one connection, `link_titles=1` must not ask for a second connection while the
/// handler holds the first (the pool has no wait timeout, so that would hang for ever).
#[tokio::test]
async fn link_titles_use_the_connection_the_handler_holds() {
    use std::sync::Arc;
    use std::time::Duration;

    let (env, reader) = setup().await;
    let name = {
        let list = env.send(get("/api/v1/resource/deal").token(&reader)).await;
        list.data()[0]["name"].as_str().unwrap().to_owned()
    };
    let pool = forgebase_db::connect_sized(&env.db.url, Some(1))
        .await
        .expect("pool of one");
    let state = forgebase_http::AppState::new(
        pool,
        Arc::new(registry_with(&[CUST, DEAL, DEAL_LINE])),
        forgebase_http::AppConfig::default(),
    );
    let app = forgebase_http::router(state);
    for uri in [
        "/api/v1/resource/deal?link_titles=1".to_owned(),
        format!("/api/v1/resource/deal/{name}?link_titles=1"),
    ] {
        let reply = tokio::time::timeout(
            Duration::from_secs(5),
            send_to(&app, get(&uri).token(&reader)),
        )
        .await
        .unwrap_or_else(|_| panic!("{uri} hung: a second connection was requested"));
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
        assert_eq!(reply.json()["link_titles"], json!({"cust": {"C-1": "Ada"}}));
    }
}
