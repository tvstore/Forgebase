//! Import and export over HTTP (M9.3, ADR 0021): the rights, the formats, the limits, the
//! per-row errors and the report export, with the status code and the envelope of each case.

mod common;

use std::sync::Arc;

use axum::http::StatusCode;
use common::{Env, Reply, Session, get, post};
use forgebase_core::document::Document;
use forgebase_core::hooks::{DocTypeHooks, HookContext, HookError};
use forgebase_core::report::{
    ColumnKind, DEFAULT_MAX_ROWS, DEFAULT_TIMEOUT, ReportColumn, ReportDef,
};
use forgebase_core::value::Value;
use forgebase_http::AppConfig;
use serde_json::{Value as Json, json};

const PARCEL: &str = r#"
name = "parcel"
label = "Parcel"
[[fields]]
name = "title"
type = "data"
required = true
[[fields]]
name = "note"
type = "text"
[[fields]]
name = "qty"
type = "int"
[[fields]]
name = "price"
type = "decimal"
precision = 12
scale = 2
[[fields]]
name = "due"
type = "date"
[[fields]]
name = "seen"
type = "datetime"
[[fields]]
name = "done"
type = "check"
[[fields]]
name = "tier"
type = "select"
options = ["gold", "silver"]
[[fields]]
name = "item"
type = "link"
target = "item"
[[fields]]
name = "lines"
type = "table"
target = "parcel_line"

[[permissions]]
role = "Exporter"
read = true
export = true
[[permissions]]
role = "Reader"
read = true
[[permissions]]
role = "Export Only"
export = true
[[permissions]]
role = "Own Exporter"
read = true
export = true
if_owner = true
[[permissions]]
role = "Importer"
read = true
create = true
import = true
[[permissions]]
role = "Import No Create"
read = true
import = true
[[permissions]]
role = "Upserter"
read = true
create = true
write = true
import = true
"#;

/// Named by its `code`: a given name is allowed when it is the code.
const TAG: &str = r#"
name = "tag"
label = "Tag"
naming = { field = "code" }
[[fields]]
name = "code"
type = "data"
required = true
unique = true
[[fields]]
name = "weight"
type = "int"
[[permissions]]
role = "Exporter"
read = true
export = true
[[permissions]]
role = "Importer"
read = true
create = true
import = true
"#;

/// A DocType without a workflow whose own field is called `workflow_state`.
const STATEFUL: &str = r#"
name = "stateful"
label = "Stateful"
[[fields]]
name = "title"
type = "data"
[[fields]]
name = "workflow_state"
type = "data"
[[permissions]]
role = "Exporter"
read = true
export = true
[[permissions]]
role = "Importer"
read = true
create = true
import = true
"#;

const PARCEL_LINE: &str = r#"
name = "parcel_line"
label = "Parcel Line"
is_child = true
[[fields]]
name = "what"
type = "data"
"#;

/// Sets the note of `hook-me`; refuses `reject`.
struct Rules;

impl DocTypeHooks for Rules {
    fn validate(&self, _: &HookContext<'_>, doc: &mut Document) -> Result<(), HookError> {
        match doc.values.get("title") {
            Some(Value::Text(t)) if t == "reject" => Err(HookError::new("rejected by the app")),
            Some(Value::Text(t)) if t == "hook-me" => {
                doc.values
                    .insert("note".into(), Value::Text("hooked".into()));
                Ok(())
            }
            _ => Ok(()),
        }
    }
}

async fn env_with(config: AppConfig) -> Env {
    let env = Env::with_hooks(
        config,
        &[PARCEL, PARCEL_LINE, TAG, STATEFUL],
        vec![("parcel", Arc::new(Rules))],
    )
    .await;
    for (user, role) in [
        ("exporter", "Exporter"),
        ("reader", "Reader"),
        ("exportonly", "Export Only"),
        ("own", "Own Exporter"),
        ("importer", "Importer"),
        ("nocreate", "Import No Create"),
        ("upserter", "Upserter"),
    ] {
        // `Clerk` may read `item`, which a link in a file must be able to resolve.
        env.user(user, &[role, "Clerk"]).await;
    }
    env.user("root", &["Administrator"]).await;
    env
}

async fn env() -> Env {
    env_with(AppConfig::default()).await
}

async fn insert(env: &Env, session: &Session, body: Json) -> Json {
    let reply = env
        .send(post("/api/v1/resource/parcel").session(session).json(body))
        .await;
    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.text);
    reply.data()
}

async fn export(env: &Env, session: &Session, query: &str) -> Reply {
    env.send(get(&format!("/api/v1/export/parcel{query}")).session(session))
        .await
}

fn csv_rows(text: &str) -> Vec<Vec<String>> {
    csv::ReaderBuilder::new()
        .has_headers(false)
        .from_reader(text.as_bytes())
        .records()
        .map(|r| r.expect("valid CSV").iter().map(str::to_owned).collect())
        .collect()
}

async fn import(
    env: &Env,
    session: &Session,
    query: &str,
    content_type: &str,
    body: &str,
) -> Reply {
    env.send(
        post(&format!("/api/v1/import/parcel{query}"))
            .session(session)
            .raw(content_type, body.as_bytes()),
    )
    .await
}

async fn titles(env: &Env, admin: &Session) -> Vec<String> {
    let reply = env
        .send(get("/api/v1/resource/parcel?sort=title&limit=500").session(admin))
        .await;
    reply
        .data()
        .as_array()
        .expect("list")
        .iter()
        .map(|d| d["title"].as_str().expect("title").to_owned())
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Export
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn export_needs_authentication_read_and_export() {
    let env = env().await;
    let root = env.login("root").await;
    insert(&env, &root, json!({"title": "a"})).await;

    // Not logged in; CSRF is not needed for a GET.
    let reply = env.send(get("/api/v1/export/parcel")).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    let exporter = env.login("exporter").await;
    let cookie_only = env
        .send(get("/api/v1/export/parcel").cookie(&exporter))
        .await;
    assert_eq!(cookie_only.status, StatusCode::OK, "{}", cookie_only.text);

    // Read without export, export without read: 403 permission_denied naming the right.
    for (user, right) in [("reader", "export"), ("exportonly", "read")] {
        let session = env.login(user).await;
        let reply = export(&env, &session, "").await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN, "{user}");
        assert_eq!(reply.code(), "permission_denied");
        assert!(
            reply.json()["error"]["message"]
                .as_str()
                .unwrap()
                .contains(right)
        );
    }
    // A DocType that does not exist (or a child DocType) is 404, like the list.
    let reply = env
        .send(get("/api/v1/export/nope").session(&exporter))
        .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    let reply = env
        .send(get("/api/v1/export/parcel_line").session(&root))
        .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    // The administrator passes, and so does an API token of a user with the rights.
    assert_eq!(export(&env, &root, "").await.status, StatusCode::OK);
    let token = env.api_token("exporter").await;
    let reply = env
        .send(get("/api/v1/export/parcel?format=json").token(&token))
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    // Without the right, nothing was streamed: a normal JSON error envelope.
    let session = env.login("reader").await;
    let reply = export(&env, &session, "?format=csv").await;
    assert!(
        reply
            .header("content-type")
            .unwrap()
            .starts_with("application/json")
    );
}

#[tokio::test]
async fn export_follows_if_owner() {
    let env = env().await;
    env.user("root2", &["Administrator"]).await;
    let root = env.login("root").await;
    let other = env.login("root2").await;
    insert(&env, &other, json!({"title": "not mine"})).await;
    // `own` is "Own Exporter": read and export on own documents; it cannot create, so the
    // administrator creates a document it owns by being it: use an Administrator named `own`.
    env.user("ownroot", &["Administrator", "Own Exporter"])
        .await;
    insert(&env, &root, json!({"title": "root's"})).await;
    let own = env.login("own").await;
    let reply = export(&env, &own, "?fields=title").await;
    assert_eq!(csv_rows(&reply.text), [["title"]]);
    assert_eq!(reply.header("x-forgebase-row-count").as_deref(), Some("0"));
    // An administrator is not limited by if_owner rules of its other roles.
    let ownroot = env.login("ownroot").await;
    let reply = export(&env, &ownroot, "?fields=title&sort=title").await;
    assert_eq!(csv_rows(&reply.text).len(), 3);
}

#[tokio::test]
async fn csv_export_headers_and_line_ends() {
    let env = env().await;
    let root = env.login("root").await;
    insert(&env, &root, json!({"title": "a"})).await;
    let reply = export(&env, &root, "?format=csv").await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(
        reply.header("content-type").as_deref(),
        Some("text/csv; charset=utf-8")
    );
    assert_eq!(reply.header("cache-control").as_deref(), Some("no-store"));
    let disposition = reply.header("content-disposition").unwrap();
    assert!(
        disposition.starts_with("attachment; filename=\"parcel-")
            && disposition.ends_with("Z.csv\""),
        "{disposition}"
    );
    assert_eq!(reply.header("x-forgebase-row-count").as_deref(), Some("1"));
    // No BOM, CRLF everywhere, a header of `name` and the fields (no child table).
    assert!(!reply.text.starts_with('\u{feff}'));
    let lines: Vec<&str> = reply.text.split("\r\n").collect();
    assert_eq!(lines.len(), 3, "{:?}", reply.text);
    assert_eq!(lines[2], "");
    assert!(!reply.text.replace("\r\n", "").contains('\n'));
    assert_eq!(
        lines[0],
        "name,title,note,qty,price,due,seen,done,tier,item"
    );
    // The default format is CSV.
    assert!(
        export(&env, &root, "")
            .await
            .header("content-type")
            .unwrap()
            .starts_with("text/csv")
    );
    // Not a format: 400.
    let reply = export(&env, &root, "?format=xml").await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(reply.code(), "invalid_query");
}

#[tokio::test]
async fn csv_cells_are_quoted_and_text_that_could_be_a_formula_is_prefixed() {
    let env = env().await;
    let root = env.login("root").await;
    env.admin_insert(&root, "item", json!({"code": "I-1"}))
        .await;
    for (title, note) in [
        ("comma, here", "x"),
        ("say \"hi\"", "x"),
        ("two\nlines", "carriage\r\nreturn"),
        (" lead", "trail "),
        ("héllo ✓ 日本", "x"),
        ("=1+1", "+cmd"),
        ("-5", "@SUM(A1)"),
        ("\tTab", "\rCR"),
        ("plain", "mid - dash"),
    ] {
        insert(&env, &root, json!({"title": title, "note": note})).await;
    }
    insert(
        &env,
        &root,
        json!({"title": "numbers", "qty": -5, "price": "-1.50", "due": "2026-10-31",
               "seen": "2026-10-31T10:20:30Z", "done": false, "tier": "gold", "item": "I-1"}),
    )
    .await;
    insert(
        &env,
        &root,
        json!({"title": "zero", "price": "0.10", "done": true}),
    )
    .await;
    let reply = export(
        &env,
        &root,
        "?fields=title,note,qty,price,due,seen,done,tier,item&sort=title",
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    let rows = csv_rows(&reply.text);
    let by_title = |title: &str| -> Vec<String> {
        rows.iter()
            .find(|r| r[0] == title)
            .unwrap_or_else(|| panic!("no row {title:?} in {rows:?}"))
            .clone()
    };
    let cells = |r: &[String]| r[1..].to_vec();
    // Quoting round-trips through a real CSV reader.
    assert_eq!(by_title("comma, here")[1], "x");
    assert_eq!(by_title("say \"hi\"")[1], "x");
    assert_eq!(by_title("two\nlines")[1], "carriage\r\nreturn");
    assert_eq!(by_title(" lead")[1], "trail ");
    assert_eq!(by_title("héllo ✓ 日本")[1], "x");
    // ... and the raw text quotes exactly what needs it.
    assert!(reply.text.contains("\"comma, here\",x,"));
    assert!(reply.text.contains("\"say \"\"hi\"\"\",x,"));
    assert!(
        reply
            .text
            .contains("\"two\nlines\",\"carriage\r\nreturn\",")
    );
    assert!(reply.text.contains("\" lead\",\"trail \","));
    assert!(reply.text.contains("héllo ✓ 日本,x,"));
    // Text that a spreadsheet would run as a formula gets a leading quote ...
    assert_eq!(by_title("'=1+1")[1], "'+cmd");
    assert_eq!(by_title("'-5")[1], "'@SUM(A1)");
    assert_eq!(by_title("'\tTab")[1], "'\rCR");
    // ... a dash in the middle does not.
    assert_eq!(by_title("plain")[1], "mid - dash");
    // Numbers, decimals, dates and booleans are never prefixed; null is empty.
    assert_eq!(
        cells(&by_title("numbers")),
        [
            "",
            "-5",
            "-1.50",
            "2026-10-31",
            "2026-10-31T10:20:30Z",
            "false",
            "gold",
            "I-1"
        ]
    );
    assert_eq!(
        cells(&by_title("zero")),
        ["", "", "0.10", "", "", "true", "", ""]
    );
}

#[tokio::test]
async fn export_applies_filters_sort_and_fields() {
    let env = env().await;
    let root = env.login("root").await;
    for (title, qty) in [("b", 2), ("a", 5), ("c", 2), ("d", 9)] {
        insert(&env, &root, json!({"title": title, "qty": qty})).await;
    }
    let filters = urlencoding("[{\"field\":\"qty\",\"op\":\"<\",\"value\":9}]");
    let reply = export(
        &env,
        &root,
        &format!("?filters={filters}&sort=qty:desc,title&fields=title,qty"),
    )
    .await;
    assert_eq!(
        csv_rows(&reply.text),
        [["title", "qty"], ["a", "5"], ["b", "2"], ["c", "2"]]
    );
    assert_eq!(reply.header("x-forgebase-row-count").as_deref(), Some("3"));
    // The standard columns can be named.
    let reply = export(&env, &root, "?fields=owner,version&filters=%5B%5D").await;
    assert_eq!(csv_rows(&reply.text)[1], ["root", "1"]);
    // Bad input is 400 invalid_query, before anything is sent.
    for query in [
        "?fields=nope".to_owned(),
        "?fields=lines".to_owned(),
        "?fields=title,title".to_owned(),
        "?fields=".to_owned(),
        "?filters=%5B%7B%22field%22%3A%22nope%22%2C%22op%22%3A%22%3D%22%2C%22value%22%3A1%7D%5D"
            .to_owned(),
        "?filters=notjson".to_owned(),
        "?sort=nope".to_owned(),
        "?limit=5".to_owned(),
        "?offset=1".to_owned(),
        "?format=csv&format=json".to_owned(),
    ] {
        let reply = export(&env, &root, &query).await;
        assert_eq!(
            reply.status,
            StatusCode::BAD_REQUEST,
            "{query}: {}",
            reply.text
        );
        assert_eq!(reply.code(), "invalid_query", "{query}");
    }
    // A table field is fine in JSON.
    let reply = export(&env, &root, "?format=json&fields=title,lines&sort=title").await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    assert_eq!(reply.json()[0], json!({"title": "a", "lines": []}));
}

fn urlencoding(text: &str) -> String {
    text.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[tokio::test]
async fn export_over_the_limit_is_422_with_the_count_and_the_limit() {
    let env = env_with(AppConfig {
        export_max_rows: 3,
        ..AppConfig::default()
    })
    .await;
    let root = env.login("root").await;
    for i in 0..4 {
        insert(&env, &root, json!({"title": format!("p{i}")})).await;
    }
    let reply = export(&env, &root, "").await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(reply.code(), "export_too_large");
    assert_eq!(
        reply.json()["error"]["details"],
        json!({"count": 4, "limit": 3})
    );
    // Narrowing the filters gets under the limit.
    let filters = urlencoding("[{\"field\":\"title\",\"op\":\"!=\",\"value\":\"p0\"}]");
    let reply = export(&env, &root, &format!("?filters={filters}")).await;
    assert_eq!(reply.status, StatusCode::OK);
}

#[tokio::test]
async fn json_export_has_child_rows_and_streams_every_page() {
    let env = env().await;
    let root = env.login("root").await;
    insert(
        &env,
        &root,
        json!({"title": "with lines", "lines": [{"what": "a"}, {"what": "b"}]}),
    )
    .await;
    let reply = export(&env, &root, "?format=json").await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(
        reply.header("content-type").as_deref(),
        Some("application/json")
    );
    assert!(
        reply
            .header("content-disposition")
            .unwrap()
            .ends_with("Z.json\"")
    );
    let documents = reply.json();
    let documents = documents
        .as_array()
        .expect("a top-level array, not an envelope");
    assert_eq!(documents.len(), 1);
    assert_eq!(documents[0]["lines"], json!([{"what": "a"}, {"what": "b"}]));
    assert_eq!(documents[0]["title"], "with lines");
    // The same shape as GET of the document.
    let one = env
        .send(
            get(&format!(
                "/api/v1/resource/parcel/{}",
                documents[0]["name"].as_str().unwrap()
            ))
            .session(&root),
        )
        .await;
    assert_eq!(one.data(), documents[0]);

    // Empty: a valid empty array.
    let env2 = env_with(AppConfig::default()).await;
    let root2 = env2.login("root").await;
    let empty = export(&env2, &root2, "?format=json").await;
    assert_eq!(empty.json(), json!([]));

    // More than two pages of 1000: complete, ordered, one array.
    env.db
        .exec(
            "INSERT INTO doc_parcel (id, name, owner, created_at, modified_at, modified_by, title) \
             SELECT gen_random_uuid(), 'S' || lpad(g::text, 5, '0'), 'seed', now(), now(), 'seed', 'T' || g \
             FROM generate_series(1, 2500) g",
        )
        .await;
    let big = export(&env, &root, "?format=json&sort=name").await;
    let all = big.json();
    assert_eq!(all.as_array().unwrap().len(), 2501);
    assert_eq!(big.header("x-forgebase-row-count").as_deref(), Some("2501"));
    let big = export(&env, &root, "?format=csv&fields=name&sort=name").await;
    assert_eq!(csv_rows(&big.text).len(), 2502);
}

/// Metadata that an import does not take back.
fn strip(d: &Json) -> Json {
    let mut d = d.clone();
    for key in [
        "id",
        "owner",
        "created_at",
        "modified_at",
        "modified_by",
        "version",
    ] {
        d.as_object_mut().unwrap().remove(key);
    }
    d
}

#[tokio::test]
async fn a_json_export_can_be_imported_again_and_gives_equal_documents() {
    // Into another site (a fresh database): a DocType named by a field takes the given names.
    let env = env().await;
    let root = env.login("root").await;
    for (code, weight) in [("T-1", 1), ("T-2", 2)] {
        env.admin_insert(&root, "tag", json!({"code": code, "weight": weight}))
            .await;
    }
    let exported = env
        .send(get("/api/v1/export/tag?format=json&sort=name").session(&root))
        .await
        .text;
    let fresh = env_with(AppConfig::default()).await;
    let importer = fresh.login("importer").await;
    let reply = fresh
        .send(
            post("/api/v1/import/tag?format=json")
                .session(&importer)
                .raw("application/json", exported.as_bytes()),
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    assert_eq!(reply.data()["inserted"], 2);
    let admin = fresh.login("root").await;
    let again = fresh
        .send(get("/api/v1/export/tag?format=json&sort=name").session(&admin))
        .await
        .json();
    let before: Json = serde_json::from_str(&exported).unwrap();
    let strip_all =
        |docs: &Json| -> Vec<Json> { docs.as_array().unwrap().iter().map(strip).collect() };
    assert_eq!(strip_all(&before), strip_all(&again));
    assert_eq!(
        strip_all(&before)[1],
        json!({"name": "T-2", "code": "T-2", "weight": 2})
    );
}

#[tokio::test]
async fn a_json_export_of_a_randomly_named_doctype_upserts_back_to_equal_documents() {
    let env = env().await;
    let root = env.login("root").await;
    env.admin_insert(&root, "item", json!({"code": "I-1"}))
        .await;
    insert(
        &env,
        &root,
        json!({"title": "one", "note": "n", "qty": 3, "price": "1.50", "due": "2026-10-31",
               "seen": "2026-10-31T10:20:30Z", "done": true, "tier": "gold", "item": "I-1",
               "lines": [{"what": "a"}, {"what": "b"}]}),
    )
    .await;
    insert(
        &env,
        &root,
        json!({"title": "two", "lines": [{"what": "c"}]}),
    )
    .await;
    let exported = export(&env, &root, "?format=json&sort=title").await.text;
    // Insert mode would give the random names: refused per row, nothing imported.
    let reply = import(&env, &root, "?format=json", "application/json", &exported).await;
    assert_eq!(reply.code(), "import_failed");
    assert!(
        reply.json()["error"]["details"]["errors"][0]["details"][0]
            .as_str()
            .unwrap()
            .contains("random naming")
    );
    // Upsert finds the documents by name and writes the same values back.
    let reply = import(
        &env,
        &root,
        "?format=json&mode=upsert",
        "application/json",
        &exported,
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    assert_eq!(
        (
            reply.data()["updated"].as_u64(),
            reply.data()["inserted"].as_u64()
        ),
        (Some(2), Some(0))
    );
    let again = export(&env, &root, "?format=json&sort=title").await.json();
    let before: Json = serde_json::from_str(&exported).unwrap();
    let docs = |v: &Json| -> Vec<Json> { v.as_array().unwrap().iter().map(strip).collect() };
    assert_eq!(docs(&before), docs(&again));
    assert_eq!(again[0]["version"], 2);
    assert_eq!(again[0]["lines"], json!([{"what": "a"}, {"what": "b"}]));
}

// ---------------------------------------------------------------------------------------------
// Import
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn import_needs_authentication_csrf_and_the_rights() {
    let env = env().await;
    let csv = "title\r\na\r\n";
    let reply = env
        .send(post("/api/v1/import/parcel?format=csv").raw("text/csv", csv.as_bytes()))
        .await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    let importer = env.login("importer").await;
    // A cookie without the CSRF token is refused.
    let reply = env
        .send(
            post("/api/v1/import/parcel?format=csv")
                .cookie(&importer)
                .raw("text/csv", csv.as_bytes()),
        )
        .await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    assert_eq!(reply.code(), "csrf_failed");
    // No import right (but export, read, create elsewhere): 403 permission_denied.
    for user in ["exporter", "reader", "exportonly", "own"] {
        let session = env.login(user).await;
        let reply = import(&env, &session, "?format=csv", "text/csv", csv).await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN, "{user}");
        assert_eq!(reply.code(), "permission_denied");
    }
    // Import without create.
    let session = env.login("nocreate").await;
    let reply = import(&env, &session, "?format=csv", "text/csv", csv).await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    assert!(
        reply.json()["error"]["message"]
            .as_str()
            .unwrap()
            .contains("create")
    );
    // An upsert needs write too.
    let reply = import(
        &env,
        &importer,
        "?format=csv&mode=upsert",
        "text/csv",
        "name,title\r\nx,a\r\n",
    )
    .await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    assert!(
        reply.json()["error"]["message"]
            .as_str()
            .unwrap()
            .contains("write")
    );
    // Import and create inserts.
    let reply = import(&env, &importer, "?format=csv", "text/csv", csv).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    // An unknown DocType is 404, a child DocType too.
    let reply = env
        .send(
            post("/api/v1/import/nope?format=csv")
                .session(&importer)
                .raw("text/csv", csv.as_bytes()),
        )
        .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn import_checks_the_parameters_and_the_content_type() {
    let env = env().await;
    let root = env.login("root").await;
    let csv = "title\r\na\r\n";
    let bad = |query: &'static str| {
        let (env, root) = (&env, &root);
        async move { import(env, root, query, "text/csv", csv).await }
    };
    for query in [
        "",
        "?format=xml",
        "?format=csv&mode=replace",
        "?format=csv&dry_run=yes",
        "?format=csv&extra=1",
        "?format=csv&format=csv",
    ] {
        let reply = bad(query).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{query}");
        assert_eq!(reply.code(), "invalid_query", "{query}");
    }
    // The type must be that of the format.
    for (query, content_type) in [
        ("?format=csv", "application/json"),
        ("?format=json", "text/csv"),
        ("?format=csv", "text/plain"),
    ] {
        let reply = import(&env, &root, query, content_type, csv).await;
        assert_eq!(
            reply.status,
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "{query} {content_type}"
        );
        assert_eq!(reply.code(), "unsupported_media_type");
    }
    // No Content-Type at all.
    let reply = env
        .send(post("/api/v1/import/parcel?format=csv").session(&root))
        .await;
    assert_eq!(reply.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    // Parameters of the media type and its case are fine; nothing was created by the refusals.
    let reply = import(&env, &root, "?format=csv", "Text/CSV; charset=utf-8", csv).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    assert_eq!(titles(&env, &root).await, ["a"]);
}

#[tokio::test]
async fn a_valid_csv_import_creates_the_documents_and_runs_the_hooks() {
    let env = env().await;
    let importer = env.login("importer").await;
    let root = env.login("root").await;
    env.admin_insert(&root, "item", json!({"code": "I-1"}))
        .await;
    let file = "\u{feff}title,note,qty,price,due,seen,done,tier,item\r\n\
                one,\"a, b\",3,1.50,2026-10-31,2026-10-31T10:20:30Z,true,gold,I-1\r\n\
                hook-me,,,,,,0,,\r\n\
                =danger,\"line\r\nbreak\",-7,-0.25,,,,silver,\r\n";
    let reply = import(&env, &importer, "?format=csv", "text/csv", file).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    let summary = reply.data();
    assert_eq!(
        summary,
        json!({"dry_run": false, "mode": "insert", "rows": 3, "inserted": 3, "updated": 0,
               "errors": [], "error_count": 0})
    );
    let listed = env
        .send(get("/api/v1/resource/parcel?sort=title&limit=50").session(&root))
        .await
        .data();
    let one = listed
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["title"] == "one")
        .unwrap();
    assert_eq!(one["note"], "a, b");
    assert_eq!(one["qty"], 3);
    assert_eq!(one["price"], "1.50");
    assert_eq!(one["done"], true);
    assert_eq!(one["item"], "I-1");
    assert_eq!(one["owner"], "importer");
    // The hook ran (note set), and an empty cell was left out.
    let hooked = listed
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["title"] == "hook-me")
        .unwrap();
    assert_eq!(hooked["note"], "hooked");
    assert_eq!(hooked["done"], false);
    assert_eq!(hooked["qty"], Json::Null);
    // CSV import does not strip the `'` an export adds, and keeps a formula-looking text as is.
    let danger = listed
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["title"] == "=danger")
        .unwrap();
    assert_eq!(danger["note"], "line\r\nbreak");
    assert_eq!(danger["qty"], -7);
}

#[tokio::test]
async fn a_dry_run_reports_and_creates_nothing() {
    let env = env().await;
    let importer = env.login("importer").await;
    let root = env.login("root").await;
    let reply = import(
        &env,
        &importer,
        "?format=csv&dry_run=1",
        "text/csv",
        "title,qty\r\na,1\r\nb,2\r\n",
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    assert_eq!(
        reply.data(),
        json!({"dry_run": true, "mode": "insert", "rows": 2, "inserted": 2, "updated": 0,
               "errors": [], "error_count": 0})
    );
    assert!(titles(&env, &root).await.is_empty());
    // With errors a dry run is still a 200 with the list; and still nothing is created.
    let reply = import(
        &env,
        &importer,
        "?format=csv&dry_run=1",
        "text/csv",
        "title,qty\r\na,1\r\nreject,2\r\n,3\r\n",
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.data()["error_count"], 2);
    assert!(titles(&env, &root).await.is_empty());
}

#[tokio::test]
async fn row_errors_are_listed_with_their_rows_and_nothing_is_imported() {
    let env = env().await;
    let importer = env.login("importer").await;
    let root = env.login("root").await;
    let file = "title,qty,due,item\r\n\
                ok,1,2026-01-01,\r\n\
                reject,1,,\r\n\
                ,2,,\r\n\
                bad,x,31/10/2026,\r\n\
                linked,1,,no-such-item\r\n\
                fine,1,,\r\n";
    let reply = import(&env, &importer, "?format=csv", "text/csv", file).await;
    assert_eq!(
        reply.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "{}",
        reply.text
    );
    assert_eq!(reply.code(), "import_failed");
    let details = reply.json()["error"]["details"].clone();
    assert_eq!(details["error_count"], 4);
    assert_eq!(details["rows"], 6);
    assert_eq!(details["dry_run"], false);
    let errors = details["errors"].as_array().unwrap();
    let rows: Vec<u64> = errors.iter().map(|e| e["row"].as_u64().unwrap()).collect();
    assert_eq!(rows, [2, 3, 4, 5]);
    // A hook refusal, a missing required field, a value that cannot be parsed, an unknown link.
    assert_eq!(errors[0]["code"], "hook_rejected");
    assert!(
        errors[0]["message"]
            .as_str()
            .unwrap()
            .contains("rejected by the app")
    );
    assert_eq!(errors[0]["line"], 3);
    assert_eq!(errors[1]["code"], "validation_failed");
    assert!(errors[1]["details"][0].as_str().unwrap().contains("title"));
    assert_eq!(errors[2]["code"], "validation_failed");
    assert_eq!(errors[2]["details"].as_array().unwrap().len(), 2);
    assert!(errors[2]["details"][0].as_str().unwrap().contains("qty"));
    assert!(errors[2]["details"][1].as_str().unwrap().contains("due"));
    assert_eq!(errors[3]["code"], "validation_failed");
    assert!(
        errors[3]["details"][0]
            .as_str()
            .unwrap()
            .contains("no-such-item")
    );
    // Nothing from the good rows was created.
    assert!(titles(&env, &root).await.is_empty());
}

#[tokio::test]
async fn a_file_with_bad_columns_is_refused_before_anything_runs() {
    let env = env().await;
    let importer = env.login("importer").await;
    let refused = |reply: &Reply| {
        assert_eq!(
            reply.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{}",
            reply.text
        );
        assert_eq!(reply.code(), "invalid_import");
        reply.json()["error"]["details"].to_string()
    };
    let reply = import(
        &env,
        &importer,
        "?format=csv",
        "text/csv",
        "title,color\r\na,red\r\n",
    )
    .await;
    assert!(refused(&reply).contains("unknown column `color`"));
    let reply = import(
        &env,
        &importer,
        "?format=csv",
        "text/csv",
        "title,title\r\na,b\r\n",
    )
    .await;
    assert!(refused(&reply).contains("duplicate column `title`"));
    let reply = import(
        &env,
        &importer,
        "?format=csv",
        "text/csv",
        "title,lines\r\na,b\r\n",
    )
    .await;
    let text = refused(&reply);
    assert!(text.contains("child tables is not supported") && text.contains("use JSON"));
    let reply = import(
        &env,
        &importer,
        "?format=csv",
        "text/csv",
        "title,qty\r\na\r\n",
    )
    .await;
    refused(&reply);
    let reply = import(
        &env,
        &importer,
        "?format=json",
        "application/json",
        "{\"title\": \"x\"}",
    )
    .await;
    refused(&reply);
    let reply = import(&env, &importer, "?format=json", "application/json", "[{").await;
    refused(&reply);
    let reply = env
        .send(
            post("/api/v1/import/parcel?format=csv")
                .session(&importer)
                .raw("text/csv", b"title\r\n\xff\xfe\r\n"),
        )
        .await;
    assert!(refused(&reply).contains("UTF-8"));
    let root = env.login("root").await;
    assert!(titles(&env, &root).await.is_empty());
}

#[tokio::test]
async fn the_row_limit_and_the_body_limit_are_the_imports_own() {
    let env = env_with(AppConfig {
        import_max_rows: 3,
        import_body_limit_bytes: 400,
        body_limit_bytes: 100,
        ..AppConfig::default()
    })
    .await;
    let importer = env.login("importer").await;
    let root = env.login("root").await;
    // More rows than the limit: 422 before anything runs.
    let reply = import(
        &env,
        &importer,
        "?format=csv",
        "text/csv",
        "title\r\na\r\nb\r\nc\r\nd\r\n",
    )
    .await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(reply.code(), "import_too_large");
    assert_eq!(reply.json()["error"]["details"], json!({"limit": 3}));
    let reply = import(
        &env,
        &importer,
        "?format=json",
        "application/json",
        r#"[{"title":"a"},{"title":"b"},{"title":"c"},{"title":"d"}]"#,
    )
    .await;
    assert_eq!(reply.code(), "import_too_large");
    // At the limit it passes.
    let reply = import(
        &env,
        &importer,
        "?format=csv",
        "text/csv",
        "title\r\na\r\nb\r\nc\r\n",
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);

    // A body over the import limit is 413; one over the ordinary limit but under the import
    // limit is fine, while the other routes keep the ordinary limit.
    let long = format!("title\r\n{}\r\n", "x".repeat(300));
    let reply = import(&env, &importer, "?format=csv&dry_run=1", "text/csv", &long).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    let huge = format!("title\r\n{}\r\n", "x".repeat(500));
    let reply = import(&env, &importer, "?format=csv", "text/csv", &huge).await;
    assert_eq!(reply.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(reply.code(), "payload_too_large");
    let reply = env
        .send(
            post("/api/v1/resource/parcel")
                .session(&root)
                .json(json!({"title": "x".repeat(300)})),
        )
        .await;
    assert_eq!(reply.status, StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn upsert_updates_existing_documents_and_inserts_new_ones() {
    let env = env().await;
    let upserter = env.login("upserter").await;
    let root = env.login("root").await;
    let existing = insert(
        &env,
        &root,
        json!({"title": "old", "qty": 1, "note": "keep"}),
    )
    .await;
    let name = existing["name"].as_str().unwrap();
    // The existing name updates (an empty cell clears); a new name is refused here because
    // `parcel` names itself, which is a row error.
    let file = format!("name,title,qty,note\r\n{name},new,,\r\n");
    let reply = import(
        &env,
        &upserter,
        "?format=csv&mode=upsert",
        "text/csv",
        &file,
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    assert_eq!(reply.data()["mode"], "upsert");
    assert_eq!(
        (
            reply.data()["updated"].as_u64(),
            reply.data()["inserted"].as_u64()
        ),
        (Some(1), Some(0))
    );
    let now = env
        .send(get(&format!("/api/v1/resource/parcel/{name}")).session(&root))
        .await
        .data();
    assert_eq!(now["title"], "new");
    assert_eq!(
        now["qty"],
        Json::Null,
        "an empty cell sets the field to null"
    );
    assert_eq!(now["note"], Json::Null);
    assert_eq!(now["version"], 2);
    // An upsert file needs a name column.
    let reply = import(
        &env,
        &upserter,
        "?format=csv&mode=upsert",
        "text/csv",
        "title\r\nx\r\n",
    )
    .await;
    assert_eq!(reply.code(), "invalid_import");
    // A JSON upsert replaces the rows it is given.
    let body = json!([{"name": name, "lines": [{"what": "z"}]}]).to_string();
    let reply = import(
        &env,
        &upserter,
        "?format=json&mode=upsert",
        "application/json",
        &body,
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    let now = env
        .send(get(&format!("/api/v1/resource/parcel/{name}")).session(&root))
        .await
        .data();
    assert_eq!(now["lines"], json!([{"what": "z"}]));
    assert_eq!(now["title"], "new");
    // A name given where naming is automatic is a row error in insert mode too.
    let file = "name,title\r\nmine,x\r\n";
    let reply = import(&env, &upserter, "?format=csv", "text/csv", file).await;
    assert_eq!(reply.code(), "import_failed");
    assert_eq!(
        reply.json()["error"]["details"]["errors"][0]["name"],
        "mine"
    );
}

// ---------------------------------------------------------------------------------------------
// Report export
// ---------------------------------------------------------------------------------------------

fn report(max_rows: u32) -> ReportDef {
    ReportDef {
        app: "t".to_owned(),
        name: "parcels".to_owned(),
        label: "Parcels".to_owned(),
        sql: "SELECT title, qty, note, due FROM doc_parcel ORDER BY title".to_owned(),
        roles: vec!["Report Reader".to_owned()],
        params: vec![],
        columns: vec![
            ReportColumn {
                name: "title".to_owned(),
                label: "Title".to_owned(),
                kind: ColumnKind::Data,
            },
            ReportColumn {
                name: "qty".to_owned(),
                label: "Qty".to_owned(),
                kind: ColumnKind::Int,
            },
            ReportColumn {
                name: "note".to_owned(),
                label: "Note".to_owned(),
                kind: ColumnKind::Data,
            },
            ReportColumn {
                name: "due".to_owned(),
                label: "Due".to_owned(),
                kind: ColumnKind::Date,
            },
        ],
        timeout: DEFAULT_TIMEOUT,
        max_rows,
    }
}

#[tokio::test]
async fn a_report_exports_for_its_roles_with_the_same_csv_rules_and_marks_a_truncated_result() {
    let env = Env::with_reports(
        &[PARCEL, PARCEL_LINE],
        vec![report(2), {
            let mut other = report(DEFAULT_MAX_ROWS);
            other.name = "all_parcels".to_owned();
            other
        }],
    )
    .await;
    env.user("rr", &["Report Reader"]).await;
    env.user("nobody", &["Reader"]).await;
    env.user("root", &["Administrator"]).await;
    let root = env.login("root").await;
    for (title, qty, note) in [("a", 1, "=1+1"), ("b", -5, "x, y"), ("c", 3, "z")] {
        env.admin_insert(
            &root,
            "parcel",
            json!({"title": title, "qty": qty, "note": note}),
        )
        .await;
    }
    let rr = env.login("rr").await;
    let url = "/api/v1/report/t.parcels/export";
    let run = |query: &'static str| {
        let (env, rr) = (&env, &rr);
        async move {
            env.send(
                post(&format!("{url}{query}"))
                    .session(rr)
                    .json(json!({"params": {}})),
            )
            .await
        }
    };
    // The role gate: a user without the role gets the same 404 as for a report that is not there.
    let nobody = env.login("nobody").await;
    let reply = env
        .send(
            post(&format!("{url}?format=csv"))
                .session(&nobody)
                .json(json!({})),
        )
        .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    let reply = env
        .send(
            post("/api/v1/report/t.nope/export")
                .session(&rr)
                .json(json!({})),
        )
        .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    let anonymous = env
        .send(post(&format!("{url}?format=csv")).json(json!({})))
        .await;
    assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED);

    // Cut at the report's own row limit (2): marked, and the file has two rows.
    let reply = run("?format=csv").await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    assert_eq!(
        reply.header("content-type").as_deref(),
        Some("text/csv; charset=utf-8")
    );
    assert_eq!(
        reply.header("x-forgebase-truncated").as_deref(),
        Some("true")
    );
    assert!(
        reply
            .header("content-disposition")
            .unwrap()
            .contains("t.parcels-")
    );
    assert_eq!(
        csv_rows(&reply.text),
        [
            ["title", "qty", "note", "due"],
            ["a", "1", "'=1+1", ""],
            ["b", "-5", "x, y", ""]
        ]
    );
    let reply = run("?format=json").await;
    assert_eq!(
        reply.header("x-forgebase-truncated").as_deref(),
        Some("true")
    );
    let body = reply.json();
    assert_eq!(body["truncated"], true);
    assert_eq!(
        body["rows"],
        json!([["a", 1, "=1+1", null], ["b", -5, "x, y", null]])
    );
    assert_eq!(
        body["columns"][0],
        json!({"name": "title", "label": "Title", "kind": "data"})
    );
    // A report within its limit has no marker.
    let reply = env
        .send(
            post("/api/v1/report/t.all_parcels/export?format=csv")
                .session(&root)
                .json(json!({})),
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.header("x-forgebase-truncated"), None);
    assert_eq!(csv_rows(&reply.text).len(), 4);
    // The parameters are checked like a run: an unknown one is a 422.
    let reply = env
        .send(
            post(&format!("{url}?format=csv"))
                .session(&rr)
                .json(json!({"params": {"nope": 1}})),
        )
        .await;
    assert_eq!(
        reply.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "{}",
        reply.text
    );
    let reply = run("?format=pdf").await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_header_with_a_hundred_thousand_columns_gets_a_short_answer() {
    let env = env().await;
    let importer = env.login("importer").await;
    let file = format!("{}\r\nx\r\n", vec!["a"; 100_000].join(","));
    let started = std::time::Instant::now();
    let reply = import(&env, &importer, "?format=csv", "text/csv", &file).await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(reply.code(), "invalid_import");
    assert!(reply.text.len() < 2_000, "{} bytes", reply.text.len());
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
    // The same with a few more columns than the DocType can have, none of them known.
    let file = format!(
        "{}\r\n",
        (0..60)
            .map(|i| format!("c{i}"))
            .collect::<Vec<_>>()
            .join(",")
    );
    let reply = import(&env, &importer, "?format=csv", "text/csv", &file).await;
    assert_eq!(reply.code(), "invalid_import");
    assert!(reply.json()["error"]["details"].as_array().unwrap().len() <= 101);
}

#[tokio::test]
async fn a_csv_with_the_standard_columns_of_an_export_imports() {
    let env = env().await;
    let importer = env.login("importer").await;
    let root = env.login("root").await;
    let file =
        "title,owner,modified_at,version,docstatus\r\nfrom export,ann,2026-01-01T00:00:00Z,4,0\r\n";
    let reply = import(&env, &importer, "?format=csv", "text/csv", file).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    assert_eq!(titles(&env, &root).await, ["from export"]);
    let owner = env
        .send(get("/api/v1/resource/parcel?limit=5").session(&root))
        .await
        .data()[0]["owner"]
        .clone();
    assert_eq!(owner, "importer", "the owner column is read, not used");
}

#[tokio::test]
async fn child_rows_count_toward_the_row_limit() {
    let env = env_with(AppConfig {
        import_max_rows: 4,
        ..AppConfig::default()
    })
    .await;
    let importer = env.login("importer").await;
    // 2 documents and 3 child rows are 5 rows.
    let body = r#"[{"title":"a","lines":[{"what":"1"},{"what":"2"}]},{"title":"b","lines":[{"what":"3"}]}]"#;
    let reply = import(&env, &importer, "?format=json", "application/json", body).await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(reply.code(), "import_too_large");
}

#[tokio::test]
async fn a_field_called_workflow_state_on_a_doctype_without_a_workflow_is_kept() {
    let env = env().await;
    let root = env.login("root").await;
    env.admin_insert(
        &root,
        "stateful",
        json!({"title": "a", "workflow_state": "open"}),
    )
    .await;
    // The export carries the value, in CSV and in JSON.
    let reply = env
        .send(get("/api/v1/export/stateful?fields=title,workflow_state").session(&root))
        .await;
    assert_eq!(
        csv_rows(&reply.text),
        [["title", "workflow_state"], ["a", "open"]]
    );
    let reply = env
        .send(get("/api/v1/export/stateful?format=json").session(&root))
        .await;
    assert_eq!(reply.json()[0]["workflow_state"], "open");
    // The import sets it, from CSV and from JSON.
    let importer = env.login("importer").await;
    let csv = "title,workflow_state\r\nb,closed\r\n";
    let reply = env
        .send(
            post("/api/v1/import/stateful?format=csv")
                .session(&importer)
                .raw("text/csv", csv.as_bytes()),
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    let body = r#"[{"title":"c","workflow_state":"waiting"}]"#;
    let reply = env
        .send(
            post("/api/v1/import/stateful?format=json")
                .session(&importer)
                .raw("application/json", body.as_bytes()),
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    let listed = env
        .send(get("/api/v1/resource/stateful?sort=title").session(&root))
        .await
        .data();
    let states: Vec<&str> = listed
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["workflow_state"].as_str().unwrap())
        .collect();
    assert_eq!(states, ["open", "closed", "waiting"]);
}

#[tokio::test]
async fn an_echoed_value_is_shortened_whatever_the_size_of_the_input() {
    let env = env().await;
    let importer = env.login("importer").await;
    let junk = "\u{1}".repeat(4 * 1024 * 1024);
    // A header, and cells of an integer, a date and a select field, each of 4 MiB of control bytes.
    let cases = [
        format!("{junk}\r\nx\r\n"),
        format!("title,qty,due,tier\r\na,{junk},{junk},{junk}\r\n"),
    ];
    for file in cases {
        let started = std::time::Instant::now();
        let reply = import(&env, &importer, "?format=csv", "text/csv", &file).await;
        assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(reply.text.len() < 16_000, "{} bytes", reply.text.len());
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
    }
    // JSON values and keys.
    let body = json!([{"title": "a", "due": junk, "price": junk, "tier": junk, junk.clone(): 1}])
        .to_string();
    let reply = import(&env, &importer, "?format=json", "application/json", &body).await;
    assert!(reply.text.len() < 16_000, "{} bytes", reply.text.len());
}
