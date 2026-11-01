//! Metadata overlays end to end (M10.1, ADR 0023): the fields an app adds to a dependency's
//! DocType, and the custom fields a site adds to its own database. Real PostgreSQL, written with
//! `forgebase::testing`, over HTTP where possible.
//!
//! The same checks run for both kinds of field, because "every path sees the field" is the
//! property: insert and update, get, list filter and sort, summary, export and import, hooks,
//! the permission rules, the metadata API and webhook snapshots. Then the parts that belong to
//! custom fields only: the Administrator-only API, disable and enable, two sites that differ,
//! live reload, the swap under load, and a stored field that no longer validates.
//!
//! Every test fails loudly (never skips) when `FORGEBASE_TEST_DATABASE_URL` is not set.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use forgebase::testing::{HttpClient, Json, TestDb, json, run_jobs};
use forgebase::{
    App, DocTypeFile, DocTypeHooks, Document, Forgebase, HookContext, HookError, OverlayFile, Value,
};

const CUSTOMER: &str = r#"
name = "customer"
label = "Customer"
[[fields]]
name = "email"
type = "data"
required = true
unique = true
[[fields]]
name = "region"
type = "data"

[[permissions]]
role = "Agent"
read = true
write = true
create = true
delete = true
export = true
import = true
[[permissions]]
role = "Viewer"
read = true
"#;

const NOTE: &str = r#"
name = "note"
label = "Note"
[[fields]]
name = "title"
type = "data"
[[fields]]
name = "lines"
type = "table"
target = "note_line"

[[permissions]]
role = "Agent"
read = true
write = true
create = true
"#;

const NOTE_LINE: &str = r#"
name = "note_line"
label = "Note Line"
is_child = true
[[fields]]
name = "text"
type = "data"
"#;

const DOCTYPES: &[DocTypeFile] = &[
    DocTypeFile::new("doctypes/customer.toml", CUSTOMER),
    DocTypeFile::new("doctypes/note.toml", NOTE),
    DocTypeFile::new("doctypes/note_line.toml", NOTE_LINE),
];

/// The overlay of the app `ext`: three fields on `customer`, the last written by a hook only.
const OVERLAY: &str = r#"
[[fields]]
name = "tier"
type = "select"
options = ["bronze", "silver", "gold"]
[[fields]]
name = "points"
type = "int"
[[fields]]
name = "badge"
type = "data"
read_only = true
"#;

const OVERLAYS: &[OverlayFile] = &[OverlayFile::new("overlays/customer.toml", OVERLAY)];

/// A hook of the app that owns the overlay: it sees `tier` and `points` and sets `badge`.
struct Badge;

impl DocTypeHooks for Badge {
    fn validate(&self, _ctx: &HookContext<'_>, doc: &mut Document) -> Result<(), HookError> {
        let gold = doc.values.get("tier") == Some(&Value::Text("gold".to_owned()));
        let many = matches!(doc.values.get("points"), Some(Value::Int(n)) if *n >= 100);
        doc.values.insert(
            "badge".to_owned(),
            if gold && many {
                Value::Text("VIP".to_owned())
            } else {
                Value::Null
            },
        );
        Ok(())
    }
}

fn crm() -> App {
    App::new("crm").doctypes(DOCTYPES)
}

fn ext() -> App {
    App::new("ext")
        .depends_on(&["crm"])
        .extend_doctype("customer", OVERLAYS)
        .hooks("customer", Badge)
}

/// The apps with the overlay.
fn with_overlay() -> Forgebase {
    Forgebase::new().app(crm()).app(ext())
}

/// The apps without it.
fn plain() -> Forgebase {
    Forgebase::new().app(crm())
}

/// Percent-encodes a query value.
fn enc(text: &str) -> String {
    let mut out = String::new();
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn filter(field: &str, op: &str, value: Json) -> String {
    enc(&json!([{"field": field, "op": op, "value": value}]).to_string())
}

/// A migrated database with `agent` (role Agent), `viewer` (Viewer), `nobody` (no role that
/// reads) and `root` (Administrator).
async fn site(binary: &Forgebase) -> TestDb {
    let db = TestDb::new().await;
    db.migrate(binary, false).await.expect("migrate");
    for (user, role) in [
        ("agent", "Agent"),
        ("viewer", "Viewer"),
        ("nobody", "Stranger"),
        ("root", "Administrator"),
    ] {
        db.create_user(user, &[role]).await;
    }
    db
}

/// The column names of a table.
async fn columns(db: &TestDb, table: &str) -> Vec<String> {
    db.rows(&format!(
        "SELECT column_name::text FROM information_schema.columns \
         WHERE table_schema = current_schema() AND table_name = '{table}' ORDER BY ordinal_position"
    ))
    .await
    .into_iter()
    .map(|r| r[0].clone())
    .collect()
}

fn names(fields: &Json) -> Vec<String> {
    fields
        .as_array()
        .expect("an array")
        .iter()
        .map(|f| f["name"].as_str().expect("a name").to_owned())
        .collect()
}

/// The fields of `GET /meta/customer`.
async fn meta_fields(client: &HttpClient) -> Vec<String> {
    let reply = client.get("/api/v1/meta/customer").await;
    assert_eq!(reply.status, 200, "{:?}", reply.body);
    names(&reply.data()["fields"])
}

// ---- the same checks for both kinds of field -----------------------------------------------------

/// Every path that reads or writes a customer, with one field `numeric` (an int), one `choice`
/// (a select with `bronze`, `silver` and `gold`) and, for the overlay, a hook that sets `badge`.
/// `with_hook` says whether the hook is there.
async fn exercise(db: &TestDb, server: &HttpClient, numeric: &str, choice: &str, with_hook: bool) {
    let agent = server.login("agent").await;
    let viewer = server.login("viewer").await;
    let nobody = server.login("nobody").await;
    let a = |email: &str, points: i64, tier: &str| json!({"email": email, "region": "north", numeric: points, choice: tier});

    // The metadata API lists the fields, after the DocType's own.
    let fields = meta_fields(&agent).await;
    assert_eq!(&fields[..2], ["email", "region"], "{fields:?}");
    assert!(fields.contains(&numeric.to_owned()) && fields.contains(&choice.to_owned()));

    // Insert, get, update.
    let created = agent
        .post("/api/v1/resource/customer", a("a@x.test", 120, "gold"))
        .await;
    assert_eq!(created.status, 201, "{:?}", created.body);
    let name = created.data()["name"].as_str().expect("name").to_owned();
    assert_eq!(created.data()[numeric], 120);
    assert_eq!(created.data()[choice], "gold");
    if with_hook {
        // The hook of the app that owns the overlay saw both fields and set the third.
        assert_eq!(created.data()["badge"], "VIP", "{:?}", created.body);
    }
    for (email, points, tier) in [("b@x.test", 30, "silver"), ("c@x.test", 60, "silver")] {
        let reply = agent
            .post("/api/v1/resource/customer", a(email, points, tier))
            .await;
        assert_eq!(reply.status, 201, "{:?}", reply.body);
    }
    let got = agent
        .get(&format!("/api/v1/resource/customer/{name}"))
        .await;
    assert_eq!(got.data()[numeric], 120);
    let version = got.data()["version"].as_i64().expect("version");
    let updated = agent
        .patch(
            &format!("/api/v1/resource/customer/{name}"),
            json!({"version": version, "data": {numeric: 150}}),
        )
        .await;
    assert_eq!(updated.status, 200, "{:?}", updated.body);
    assert_eq!(updated.data()[numeric], 150);
    if with_hook {
        assert_eq!(updated.data()["badge"], "VIP");
    }
    // Validation applies like for any field: a value that is not an option is refused.
    let bad = agent
        .post("/api/v1/resource/customer", a("d@x.test", 1, "platinum"))
        .await;
    assert_eq!(bad.status, 422, "{:?}", bad.body);

    // List: filter, sort.
    let list = agent
        .get(&format!(
            "/api/v1/resource/customer?filters={}&sort={}",
            filter(numeric, ">=", json!(50)),
            enc(&format!("{numeric}:desc"))
        ))
        .await;
    assert_eq!(list.status, 200, "{:?}", list.body);
    let points: Vec<i64> = list
        .data()
        .as_array()
        .expect("list")
        .iter()
        .map(|d| d[numeric].as_i64().expect("points"))
        .collect();
    assert_eq!(points, [150, 60]);

    // Summary: group by the select, sum the int.
    let summary = agent
        .get(&format!(
            "/api/v1/summary/customer?group_by={choice}&aggregates={}",
            enc(&format!("sum:{numeric}"))
        ))
        .await;
    assert_eq!(summary.status, 200, "{:?}", summary.body);
    let groups = summary.data()["groups"].as_array().expect("groups");
    let silver = groups
        .iter()
        .find(|g| g["key"] == "silver")
        .expect("silver");
    assert_eq!(silver["count"], 2);
    assert_eq!(silver["values"][format!("sum_{numeric}")], 90);

    // Export includes the field; an import sets it.
    let export = agent
        .get(&format!(
            "/api/v1/export/customer?format=json&sort={}",
            enc(&format!("{numeric}:asc"))
        ))
        .await;
    assert_eq!(export.status, 200, "{:?}", export.body);
    let rows = export.body.as_array().expect("an array");
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0][numeric], 30);
    let import = agent
        .post_raw(
            "/api/v1/import/customer?format=json&mode=insert",
            Some("application/json"),
            json!([{"email": "i@x.test", numeric: 77, choice: "bronze"}])
                .to_string()
                .as_bytes(),
        )
        .await;
    assert_eq!(import.status, 200, "{:?}", import.body);
    let imported = agent
        .get(&format!(
            "/api/v1/resource/customer?filters={}",
            filter(numeric, "=", json!(77))
        ))
        .await;
    assert_eq!(imported.data().as_array().expect("list").len(), 1);

    // The permission rules apply as for any field: a Viewer reads it and cannot write it, a
    // caller with no right on the DocType gets nothing.
    let seen = viewer
        .get(&format!("/api/v1/resource/customer/{name}"))
        .await;
    assert_eq!(seen.data()[numeric], 150);
    let refused = viewer
        .patch(
            &format!("/api/v1/resource/customer/{name}"),
            json!({"version": 99, "data": {numeric: 1}}),
        )
        .await;
    assert_eq!(refused.status, 403, "{:?}", refused.body);
    assert_eq!(
        nobody
            .get(&format!("/api/v1/resource/customer/{name}"))
            .await
            .status,
        403
    );
    assert_eq!(nobody.get("/api/v1/meta/customer").await.status, 403);

    // A webhook snapshot has the field.
    db.exec(
        "INSERT INTO fb_webhook (name, doctype, events, url, secret, allow_private, insecure_http) \
         VALUES ('hook', 'customer', '{insert}', 'http://127.0.0.1:9/h', 'secret', true, true)",
    )
    .await;
    let hooked = agent
        .post("/api/v1/resource/customer", a("w@x.test", 5, "bronze"))
        .await;
    assert_eq!(hooked.status, 201, "{:?}", hooked.body);
    let snapshot = db
        .rows(&format!(
            "SELECT payload::jsonb->'data'->>'{numeric}' FROM fb_job WHERE name = 'forgebase.deliver_webhook'"
        ))
        .await;
    assert_eq!(snapshot, vec![vec!["5".to_owned()]]);
}

#[tokio::test]
async fn an_app_overlay_field_is_seen_by_every_path() {
    let binary = with_overlay();
    let db = site(&binary).await;
    // `migrate` added the columns.
    let columns: Vec<String> = columns(&db, "doc_customer").await;
    for column in ["tier", "points", "badge"] {
        assert!(columns.contains(&column.to_owned()), "{columns:?}");
    }
    let server = db.client(&binary);
    exercise(&db, &server, "points", "tier", true).await;

    // The field `badge` is written by the hook only: caller input for it is refused.
    let agent = server.login("agent").await;
    let refused = agent
        .post(
            "/api/v1/resource/customer",
            json!({"email": "z@x.test", "badge": "VIP"}),
        )
        .await;
    assert_eq!(refused.status, 422, "{:?}", refused.body);
    // The meta API has the owner app of the DocType, as before.
    let meta = agent.get("/api/v1/meta/customer").await;
    assert_eq!(meta.data()["app"], "crm");
    db.cleanup().await;
}

#[tokio::test]
async fn without_the_overlay_nothing_changes() {
    let binary = plain();
    let db = site(&binary).await;
    let agent = db.client(&binary).login("agent").await;
    assert_eq!(meta_fields(&agent).await, ["email", "region"]);
    let columns: Vec<String> = columns(&db, "doc_customer").await;
    assert!(!columns.contains(&"tier".to_owned()));
    let reply = agent
        .post(
            "/api/v1/resource/customer",
            json!({"email": "a@x.test", "tier": "gold"}),
        )
        .await;
    assert_eq!(reply.status, 422, "{:?}", reply.body);
    db.cleanup().await;
}

#[tokio::test]
async fn a_site_custom_field_is_seen_by_every_path() {
    let binary = plain();
    let db = site(&binary).await;
    let server = db.client(&binary);
    let root = server.login("root").await;
    for body in [
        json!({"name": "custom_points", "type": "int"}),
        json!({"name": "custom_tier", "type": "select", "options": ["bronze", "silver", "gold"]}),
    ] {
        let reply = root.post("/api/v1/customize/customer/fields", body).await;
        assert_eq!(reply.status, 201, "{:?}", reply.body);
    }
    // The server that took the change sees it at once, with no reload interval to wait for.
    exercise(&db, &server, "custom_points", "custom_tier", false).await;
    db.cleanup().await;
}

// ---- disable and enable ---------------------------------------------------------------------

#[tokio::test]
async fn a_disabled_field_is_hidden_everywhere_and_its_data_is_kept() {
    let binary = plain();
    let db = site(&binary).await;
    let server = db.client(&binary);
    let root = server.login("root").await;
    let agent = server.login("agent").await;
    let add = root
        .post(
            "/api/v1/customize/customer/fields",
            json!({"name": "custom_vip", "type": "check"}),
        )
        .await;
    assert_eq!(add.status, 201, "{:?}", add.body);
    let created = agent
        .post(
            "/api/v1/resource/customer",
            json!({"email": "a@x.test", "custom_vip": true}),
        )
        .await;
    assert_eq!(created.status, 201, "{:?}", created.body);
    let name = created.data()["name"].as_str().expect("name").to_owned();

    let disabled = root
        .post(
            "/api/v1/customize/customer/fields/custom_vip/disable",
            json!({}),
        )
        .await;
    assert_eq!(disabled.status, 200, "{:?}", disabled.body);
    assert_eq!(disabled.data()["enabled"], false);

    // Hidden: the meta API, the document, input, filters, sort, summary and export.
    assert!(!meta_fields(&agent).await.contains(&"custom_vip".to_owned()));
    let got = agent
        .get(&format!("/api/v1/resource/customer/{name}"))
        .await;
    assert!(got.data().get("custom_vip").is_none(), "{:?}", got.body);
    let version = got.data()["version"].as_i64().expect("version");
    let refused = agent
        .patch(
            &format!("/api/v1/resource/customer/{name}"),
            json!({"version": version, "data": {"custom_vip": false}}),
        )
        .await;
    assert_eq!(refused.status, 422, "{:?}", refused.body);
    assert_eq!(
        agent
            .post(
                "/api/v1/resource/customer",
                json!({"email": "b@x.test", "custom_vip": true})
            )
            .await
            .status,
        422
    );
    let filtered = agent
        .get(&format!(
            "/api/v1/resource/customer?filters={}",
            filter("custom_vip", "=", json!(true))
        ))
        .await;
    assert_eq!(filtered.status, 400, "{:?}", filtered.body);
    assert_eq!(filtered.error_code(), Some("invalid_query"));
    assert_eq!(
        agent
            .get("/api/v1/resource/customer?sort=custom_vip")
            .await
            .status,
        400
    );
    assert_eq!(
        agent
            .get("/api/v1/summary/customer?group_by=custom_vip")
            .await
            .status,
        400
    );
    let export = agent.get("/api/v1/export/customer?format=json").await;
    assert_eq!(export.status, 200, "{:?}", export.body);
    assert!(
        export.body[0].get("custom_vip").is_none(),
        "{:?}",
        export.body
    );
    assert_eq!(
        agent
            .get("/api/v1/export/customer?format=json&fields=custom_vip")
            .await
            .status,
        400
    );
    // The data is still in the column.
    assert_eq!(
        db.rows("SELECT custom_vip::text FROM doc_customer").await,
        vec![vec!["true".to_owned()]]
    );
    // The list still shows it, disabled.
    let listed = root.get("/api/v1/customize/customer/fields").await;
    assert_eq!(listed.data()[0]["enabled"], false);

    // Enabled again: the old value is back.
    let enabled = root
        .post(
            "/api/v1/customize/customer/fields/custom_vip/enable",
            json!({}),
        )
        .await;
    assert_eq!(enabled.status, 200, "{:?}", enabled.body);
    let got = agent
        .get(&format!("/api/v1/resource/customer/{name}"))
        .await;
    assert_eq!(got.data()["custom_vip"], true);
    assert!(meta_fields(&agent).await.contains(&"custom_vip".to_owned()));
    db.cleanup().await;
}

// ---- the Administrator-only API -------------------------------------------------------------------

#[tokio::test]
async fn the_customize_api_is_for_administrators_and_validates() {
    let binary = plain();
    let db = site(&binary).await;
    let server = db.client(&binary);
    let root = server.login("root").await;
    let post = |client: &HttpClient, doctype: &str, body: Json| {
        let (client, path) = (
            client.clone(),
            format!("/api/v1/customize/{doctype}/fields"),
        );
        async move { client.post(&path, body).await }
    };

    // A non-administrator, with a right on the DocType or not, gets 403 on every route; the
    // cookie of an administrator without the CSRF header gets 403 too.
    let ok = json!({"name": "custom_a", "type": "check"});
    for user in ["agent", "viewer", "nobody"] {
        let client = server.login(user).await;
        let list = client.get("/api/v1/customize/customer/fields").await;
        assert_eq!(
            (list.status, list.error_code()),
            (403, Some("permission_denied")),
            "{user}"
        );
        let add = post(&client, "customer", ok.clone()).await;
        assert_eq!(
            (add.status, add.error_code()),
            (403, Some("permission_denied")),
            "{user}"
        );
        for action in ["disable", "enable"] {
            let reply = client
                .post(
                    &format!("/api/v1/customize/customer/fields/custom_a/{action}"),
                    json!({}),
                )
                .await;
            assert_eq!(reply.status, 403, "{user} {action}");
        }
    }
    let no_csrf = root.without_csrf();
    let reply = post(&no_csrf, "customer", ok.clone()).await;
    assert_eq!(
        (reply.status, reply.error_code()),
        (403, Some("csrf_failed"))
    );
    // No credentials: 401.
    assert_eq!(
        db.client(&binary)
            .get("/api/v1/customize/customer/fields")
            .await
            .status,
        401
    );
    // Nothing was added by any of the above.
    assert!(
        root.get("/api/v1/customize/customer/fields")
            .await
            .data()
            .as_array()
            .expect("array")
            .is_empty()
    );

    // An API token of an administrator works like a session, without CSRF.
    let token = db.create_token("root").await;
    let by_token = db.client(&binary).with_token(&token);
    let reply = post(
        &by_token,
        "customer",
        json!({"name": "custom_t", "type": "int"}),
    )
    .await;
    assert_eq!(reply.status, 201, "{:?}", reply.body);
    assert_eq!(reply.data()["modified_by"], "root");
    assert_eq!(reply.data()["enabled"], true);
    assert_eq!(reply.data()["type"], "int");

    // The administrator's rules.
    let rules: Vec<(&str, Json, u16)> = vec![
        ("customer", json!({"name": "vip", "type": "check"}), 422),
        ("customer", json!({"name": "custom_", "type": "check"}), 422),
        (
            "customer",
            json!({"name": "custom_a", "type": "table", "target": "note_line"}),
            422,
        ),
        (
            "customer",
            json!({"name": "custom_a", "type": "check", "required": true}),
            422,
        ),
        (
            "customer",
            json!({"name": "custom_a", "type": "check", "read_only": true}),
            422,
        ),
        (
            "customer",
            json!({"name": "custom_a", "type": "link", "target": "ghost"}),
            422,
        ),
        (
            "customer",
            json!({"name": "custom_a", "type": "link", "target": "note_line"}),
            422,
        ),
        ("customer", json!({"name": "custom_a", "type": "nope"}), 422),
        (
            "customer",
            json!({"name": "custom_a", "type": "check", "colour": "red"}),
            422,
        ),
        ("customer", json!({"type": "check"}), 422),
        ("customer", json!([1, 2]), 422),
        (
            "note_line",
            json!({"name": "custom_a", "type": "check"}),
            422,
        ),
        ("ghost", json!({"name": "custom_a", "type": "check"}), 404),
        (
            "customer",
            json!({"name": "custom_t", "type": "check"}),
            409,
        ),
        ("customer", json!({"name": "email", "type": "check"}), 422),
    ];
    for (doctype, body, status) in rules {
        let reply = post(&root, doctype, body.clone()).await;
        assert_eq!(reply.status, status, "{doctype} {body}: {:?}", reply.body);
    }
    // A disabled field still holds its name.
    root.post(
        "/api/v1/customize/customer/fields/custom_t/disable",
        json!({}),
    )
    .await;
    assert_eq!(
        post(
            &root,
            "customer",
            json!({"name": "custom_t", "type": "check"})
        )
        .await
        .status,
        409
    );
    // Unknown things are 404.
    assert_eq!(
        root.post(
            "/api/v1/customize/customer/fields/custom_nope/disable",
            json!({})
        )
        .await
        .status,
        404
    );
    assert_eq!(root.get("/api/v1/customize/ghost/fields").await.status, 404);
    // A child DocType can be listed (it exists) and has no fields.
    assert_eq!(
        root.get("/api/v1/customize/note_line/fields").await.status,
        200
    );
    // The refused ones left nothing behind.
    let listed = root.get("/api/v1/customize/customer/fields").await;
    assert_eq!(names(&listed.data().clone()), ["custom_t"]);
    db.cleanup().await;
}

// ---- two sites, live reload ----------------------------------------------------------------------

#[tokio::test]
async fn two_sites_with_different_custom_fields_stay_apart() {
    let binary = plain();
    let (a, b) = (site(&binary).await, site(&binary).await);
    let (root_a, root_b) = (
        a.client(&binary).login("root").await,
        b.client(&binary).login("root").await,
    );
    assert_eq!(
        root_a
            .post(
                "/api/v1/customize/customer/fields",
                json!({"name": "custom_a", "type": "int"})
            )
            .await
            .status,
        201
    );
    assert_eq!(
        root_b
            .post(
                "/api/v1/customize/customer/fields",
                json!({"name": "custom_b", "type": "data"})
            )
            .await
            .status,
        201
    );
    let (agent_a, agent_b) = (
        a.client(&binary).login("agent").await,
        b.client(&binary).login("agent").await,
    );
    // The clients above started before nothing else changed: their registries are theirs.
    agent_a.reload_metadata().await;
    agent_b.reload_metadata().await;
    assert_eq!(meta_fields(&agent_a).await, ["email", "region", "custom_a"]);
    assert_eq!(meta_fields(&agent_b).await, ["email", "region", "custom_b"]);
    let in_a = agent_a
        .post(
            "/api/v1/resource/customer",
            json!({"email": "x@x.test", "custom_a": 1}),
        )
        .await;
    assert_eq!(in_a.status, 201, "{:?}", in_a.body);
    assert!(in_a.data().get("custom_b").is_none());
    // A field of site A is unknown on site B, in input and in filters.
    assert_eq!(
        agent_b
            .post(
                "/api/v1/resource/customer",
                json!({"email": "x@x.test", "custom_a": 1})
            )
            .await
            .status,
        422
    );
    assert_eq!(
        agent_b
            .get(&format!(
                "/api/v1/resource/customer?filters={}",
                filter("custom_a", "=", json!(1))
            ))
            .await
            .status,
        400
    );
    let in_b = agent_b
        .post(
            "/api/v1/resource/customer",
            json!({"email": "x@x.test", "custom_b": "hi"}),
        )
        .await;
    assert_eq!(in_b.status, 201, "{:?}", in_b.body);
    // Their lists show only their own fields.
    let list_a = agent_a.get("/api/v1/resource/customer").await;
    assert!(
        list_a.data()[0].get("custom_a").is_some() && list_a.data()[0].get("custom_b").is_none()
    );
    let list_b = agent_b.get("/api/v1/resource/customer").await;
    assert!(
        list_b.data()[0].get("custom_b").is_some() && list_b.data()[0].get("custom_a").is_none()
    );
    // The columns are in the right database only.
    assert_eq!(
        a.rows("SELECT count(*)::text FROM information_schema.columns WHERE table_name = 'doc_customer' AND column_name LIKE 'custom_%'").await,
        vec![vec!["1".to_owned()]]
    );
    a.cleanup().await;
    b.cleanup().await;
}

/// Waits (at most `limit`) until `condition` holds.
async fn until<F, Fut>(limit: Duration, mut condition: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let start = Instant::now();
    while start.elapsed() < limit {
        if condition().await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    false
}

#[tokio::test]
async fn a_server_sees_a_field_added_elsewhere_within_the_reload_interval() {
    let binary = plain();
    let db = site(&binary).await;
    // A long interval: this server does not look in the test's time, so the first half is
    // deterministic. The other polls every 100 ms.
    let slow = db
        .client_polling(&binary, Duration::from_secs(3600))
        .login("agent")
        .await;
    let fast = db
        .client_polling(&binary, Duration::from_millis(100))
        .login("agent")
        .await;
    assert_eq!(meta_fields(&fast).await, ["email", "region"]);

    // Another process (here: the command's function) adds the field.
    db.add_custom_field(
        &binary,
        "customer",
        "name = \"custom_vip\"\ntype = \"check\"\n",
    )
    .await;

    assert_eq!(
        meta_fields(&slow).await,
        ["email", "region"],
        "no poll has run: the registry is unchanged"
    );
    let reply = slow
        .post(
            "/api/v1/resource/customer",
            json!({"email": "a@x.test", "custom_vip": true}),
        )
        .await;
    assert_eq!(reply.status, 422, "{:?}", reply.body);
    assert!(
        until(Duration::from_secs(10), || async {
            meta_fields(&fast).await.contains(&"custom_vip".to_owned())
        })
        .await,
        "the polling server never saw the field"
    );
    assert_eq!(
        fast.post(
            "/api/v1/resource/customer",
            json!({"email": "a@x.test", "custom_vip": true})
        )
        .await
        .status,
        201
    );
    // The forced reload is the testing helper for the same thing.
    slow.reload_metadata().await;
    assert!(meta_fields(&slow).await.contains(&"custom_vip".to_owned()));

    // Disabling is seen the same way.
    db.set_custom_field_enabled("customer", "custom_vip", false)
        .await;
    assert!(
        until(Duration::from_secs(10), || async {
            !meta_fields(&fast).await.contains(&"custom_vip".to_owned())
        })
        .await
    );
    db.cleanup().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn swapping_the_registry_under_load_fails_no_request() {
    let binary = plain();
    let db = site(&binary).await;
    db.add_custom_field(&binary, "customer", "name = \"custom_a\"\ntype = \"int\"\n")
        .await;
    // A very short interval, so the poll swaps all the time besides the forced reloads.
    let agent = db
        .client_polling(&binary, Duration::from_millis(5))
        .login("agent")
        .await;
    agent.reload_metadata().await;
    let first = agent
        .post(
            "/api/v1/resource/customer",
            json!({"email": "seed@x.test", "custom_a": 1}),
        )
        .await;
    assert_eq!(first.status, 201, "{:?}", first.body);

    let stop = Arc::new(AtomicBool::new(false));
    let mut tasks = Vec::new();
    for worker in 0..6_usize {
        let (client, stop) = (agent.clone(), Arc::clone(&stop));
        tasks.push(tokio::spawn(async move {
            let mut requests = 0_u64;
            while !stop.load(Ordering::Relaxed) {
                // A list: every row of the answer has the field or none has (one registry).
                let list = client.get("/api/v1/resource/customer").await;
                assert_eq!(list.status, 200, "{:?}", list.body);
                let rows = list.data().as_array().expect("rows");
                let with = rows.iter().filter(|r| r.get("custom_a").is_some()).count();
                assert!(
                    with == 0 || with == rows.len(),
                    "mixed registry in one answer"
                );
                // The meta: the field is there or not, never half of anything.
                let meta = client.get("/api/v1/meta/customer").await;
                assert_eq!(meta.status, 200, "{:?}", meta.body);
                let custom = names(&meta.data()["fields"])
                    .into_iter()
                    .filter(|n| n.starts_with("custom_"))
                    .count();
                assert!(custom <= 1);
                // A write that does not use the field works whatever the registry is.
                let reply = client
                    .post(
                        "/api/v1/resource/customer",
                        json!({"email": format!("w{worker}-{requests}@x.test")}),
                    )
                    .await;
                assert_eq!(reply.status, 201, "{:?}", reply.body);
                // A write with the field either succeeds (enabled) or is a clean 422 (disabled).
                let reply = client
                    .post(
                        "/api/v1/resource/customer",
                        json!({"email": format!("f{worker}-{requests}@x.test"), "custom_a": 2}),
                    )
                    .await;
                assert!(
                    matches!(reply.status, 201 | 422),
                    "{} {:?}",
                    reply.status,
                    reply.body
                );
                requests += 1;
            }
            requests
        }));
    }
    for round in 0..20 {
        db.set_custom_field_enabled("customer", "custom_a", round % 2 == 1)
            .await;
        agent.reload_metadata().await;
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    stop.store(true, Ordering::Relaxed);
    let mut total = 0;
    for task in tasks {
        total += task.await.expect("no assertion failed in a request task");
    }
    assert!(total > 0);
    db.cleanup().await;
}

// ---- stored fields that no longer validate, the worker, migrate -------------------------------------

#[tokio::test]
async fn a_stored_field_that_no_longer_validates_is_skipped_and_the_site_serves() {
    let binary = plain();
    let db = site(&binary).await;
    db.add_custom_field(
        &binary,
        "customer",
        "name = \"custom_ok\"\ntype = \"int\"\n",
    )
    .await;
    // Its Link target left the apps; written around the framework.
    db.exec(
        "INSERT INTO fb_custom_field (doctype, name, definition, modified_by) VALUES \
         ('customer', 'custom_gone', '{\"name\":\"custom_gone\",\"type\":\"link\",\"target\":\"ghost\"}', 'sql')",
    )
    .await;
    let client = db.client(&binary).login("agent").await;
    client.reload_metadata().await;
    assert_eq!(meta_fields(&client).await, ["email", "region", "custom_ok"]);
    let reply = client
        .post(
            "/api/v1/resource/customer",
            json!({"email": "a@x.test", "custom_ok": 3}),
        )
        .await;
    assert_eq!(reply.status, 201, "{:?}", reply.body);
    // `migrate` still works with the bad row in place.
    db.migrate(&binary, false).await.expect("migrate");
    db.cleanup().await;
}

#[tokio::test]
async fn migrate_recreates_a_missing_custom_column() {
    let binary = plain();
    let db = site(&binary).await;
    db.add_custom_field(&binary, "customer", "name = \"custom_a\"\ntype = \"int\"\n")
        .await;
    db.exec("ALTER TABLE doc_customer DROP COLUMN custom_a")
        .await;
    db.migrate(&binary, false).await.expect("migrate");
    let columns: Vec<String> = columns(&db, "doc_customer").await;
    assert!(columns.contains(&"custom_a".to_owned()), "{columns:?}");
    db.cleanup().await;
}

/// A job handler that writes a custom field: it sees the registry of the worker's site.
struct Tag;

impl forgebase::JobHandler for Tag {
    fn run<'a>(&'a self, ctx: &'a mut forgebase::JobContext<'_>) -> forgebase::JobFuture<'a> {
        Box::pin(async move {
            let doc = Document {
                values: [
                    ("email".to_owned(), Value::Text("job@x.test".to_owned())),
                    ("custom_a".to_owned(), Value::Int(9)),
                ]
                .into(),
                ..Document::default()
            };
            ctx.documents().insert("customer", doc).await?;
            Ok(())
        })
    }
}

#[tokio::test]
async fn a_job_sees_the_custom_fields_of_its_site() {
    let binary = Forgebase::new().app(crm().job("tag", Tag));
    let db = site(&binary).await;
    db.add_custom_field(&binary, "customer", "name = \"custom_a\"\ntype = \"int\"\n")
        .await;
    db.exec(
        "INSERT INTO fb_job (id, name, payload, status, enqueued_by) \
         VALUES (gen_random_uuid(), 'crm.tag', '{}', 'queued', 'test')",
    )
    .await;
    let run = run_jobs(&binary, &db).await;
    assert_eq!((run.done(), run.dead()), (1, 0));
    assert_eq!(
        db.rows("SELECT custom_a::text FROM doc_customer WHERE email = 'job@x.test'")
            .await,
        vec![vec!["9".to_owned()]]
    );
    db.cleanup().await;
}

/// A schema change waits for the migrate lock; when `migrate` holds it longer than the
/// `lock_timeout`, the answer is `503 lock_timeout` with `Retry-After`, nothing is written, and the
/// server still works.
#[tokio::test]
async fn a_busy_migrate_lock_is_a_503_that_changes_nothing() {
    let binary = plain();
    let db = site(&binary).await;
    let server = db.client(&binary);
    let root = server.login("root").await;
    // Another session holds the lock of `migrate` (the key of `MIGRATE_LOCK_KEY`).
    let holder = forgebase_db::connect(db.url()).await.expect("pool");
    let held = holder.get().await.expect("connection");
    held.execute("SELECT pg_advisory_lock(5062693908242366465)", &[])
        .await
        .expect("lock");
    let busy = root
        .post(
            "/api/v1/customize/customer/fields",
            json!({"name": "custom_a", "type": "int"}),
        )
        .await;
    assert_eq!(busy.status, 503, "{:?}", busy.body);
    assert_eq!(busy.error_code(), Some("lock_timeout"));
    held.execute("SELECT pg_advisory_unlock(5062693908242366465)", &[])
        .await
        .expect("unlock");
    assert!(
        root.get("/api/v1/customize/customer/fields")
            .await
            .data()
            .as_array()
            .expect("array")
            .is_empty()
    );
    // Once the lock is free the same call works.
    let ok = root
        .post(
            "/api/v1/customize/customer/fields",
            json!({"name": "custom_a", "type": "int"}),
        )
        .await;
    assert_eq!(ok.status, 201, "{:?}", ok.body);
    drop(held);
    db.cleanup().await;
}
