//! Field permission levels end to end (M10.2, ADR 0024), with `forgebase::testing`: an app
//! overlay field and a site custom field that carry a `perm_level`, how they behave for callers
//! with and without the level, after a live reload and when they are disabled and enabled, and
//! the documents handle of an app method (the caller's rights apply there too).
//!
//! Every test fails loudly (never skips) when `FORGEBASE_TEST_DATABASE_URL` is not set.

use std::time::{Duration, Instant};

use forgebase::serde_json::json;
use forgebase::testing::{HttpClient, Json, TestDb};
use forgebase::{
    App, DocTypeFile, Forgebase, Method, MethodContext, MethodFuture, MethodHandler, OverlayFile,
};

/// `Manager` reads and writes level 1, `Agent` is a plain clerk, `Auditor` reads level 1.
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
[[permissions]]
role = "Manager"
read = true
write = true
create = true
[[permissions]]
role = "Manager"
level = 1
read = true
write = true
[[permissions]]
role = "Auditor"
read = true
[[permissions]]
role = "Auditor"
level = 1
read = true
"#;

const DOCTYPES: &[DocTypeFile] = &[DocTypeFile::new("doctypes/customer.toml", CUSTOMER)];

/// The overlay of the app `ext`: a field at level 1.
const OVERLAY: &str = r#"
[[fields]]
name = "credit_limit"
type = "int"
perm_level = 1
"#;

const OVERLAYS: &[OverlayFile] = &[OverlayFile::new("overlays/customer.toml", OVERLAY)];

/// `crm.peek`: the keys and the `credit_limit` of the customer `name` as the caller reads it.
struct Peek;

impl MethodHandler for Peek {
    fn call<'a>(&'a self, ctx: &'a mut MethodContext<'_>) -> MethodFuture<'a> {
        Box::pin(async move {
            let name = ctx.body()["name"].as_str().unwrap_or_default().to_owned();
            let found = ctx.documents().get("customer", &name).await?;
            Ok(match found {
                Some(stored) => {
                    let keys: Vec<&String> = stored.document().values.keys().collect();
                    json!({"keys": keys})
                }
                None => json!({"keys": null}),
            })
        })
    }
}

fn crm() -> App {
    App::new("crm")
        .doctypes(DOCTYPES)
        .method("peek", Method::new(Peek).any_user())
}

fn ext() -> App {
    App::new("ext")
        .depends_on(&["crm"])
        .extend_doctype("customer", OVERLAYS)
}

fn binary() -> Forgebase {
    Forgebase::new().app(crm()).app(ext())
}

fn plain() -> Forgebase {
    Forgebase::new().app(crm())
}

async fn site(binary: &Forgebase) -> TestDb {
    let db = TestDb::new().await;
    db.migrate(binary, false).await.expect("migrate");
    for (user, roles) in [
        ("agent", vec!["Agent"]),
        ("manager", vec!["Manager"]),
        ("auditor", vec!["Auditor", "Agent"]),
        ("root", vec!["Administrator"]),
    ] {
        db.create_user(user, &roles).await;
    }
    db
}

fn keys(data: &Json) -> Vec<String> {
    data.as_object()
        .expect("an object")
        .keys()
        .cloned()
        .collect()
}

async fn meta_names(client: &HttpClient) -> Vec<String> {
    let reply = client.get("/api/v1/meta/customer").await;
    assert_eq!(reply.status, 200, "{:?}", reply.body);
    reply.data()["fields"]
        .as_array()
        .expect("fields")
        .iter()
        .map(|f| f["name"].as_str().expect("name").to_owned())
        .collect()
}

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
async fn an_overlay_field_with_a_level_follows_the_rules_of_the_target() {
    let binary = binary();
    let db = site(&binary).await;
    let server = db.client(&binary);
    let (agent, manager, auditor, root) = (
        server.login("agent").await,
        server.login("manager").await,
        server.login("auditor").await,
        server.login("root").await,
    );
    let created = manager
        .post(
            "/api/v1/resource/customer",
            json!({"email": "a@x.test", "region": "n", "credit_limit": 500}),
        )
        .await;
    assert_eq!(created.status, 201, "{:?}", created.body);
    assert_eq!(created.data()["credit_limit"], 500);
    let name = created.data()["name"].as_str().expect("name").to_owned();
    let version = created.data()["version"].as_i64().expect("version");

    // The agent has no level 1: nothing of the field anywhere.
    let got = agent
        .get(&format!("/api/v1/resource/customer/{name}"))
        .await;
    assert!(!keys(got.data()).contains(&"credit_limit".to_owned()));
    assert_eq!(meta_names(&agent).await, ["email", "region"]);
    assert_eq!(
        meta_names(&manager).await,
        ["email", "region", "credit_limit"]
    );
    let refused = agent
        .post(
            "/api/v1/resource/customer",
            json!({"email": "b@x.test", "credit_limit": 1}),
        )
        .await;
    assert_eq!(refused.status, 422);
    assert_eq!(refused.error_code(), Some("validation_failed"));
    assert_eq!(
        refused.body["error"]["details"],
        json!(["unknown field `credit_limit`"])
    );
    // An update by the agent leaves the value.
    let updated = agent
        .patch(
            &format!("/api/v1/resource/customer/{name}"),
            json!({"version": version, "data": {"region": "s"}}),
        )
        .await;
    assert_eq!(updated.status, 200, "{:?}", updated.body);
    assert!(!keys(updated.data()).contains(&"credit_limit".to_owned()));
    let all = root.get(&format!("/api/v1/resource/customer/{name}")).await;
    assert_eq!(all.data()["credit_limit"], 500);

    // The auditor reads but may not write the field.
    let seen = auditor
        .get(&format!("/api/v1/resource/customer/{name}"))
        .await;
    assert_eq!(seen.data()["credit_limit"], 500);
    let denied = auditor
        .patch(
            &format!("/api/v1/resource/customer/{name}"),
            json!({"version": 2, "data": {"credit_limit": 1}}),
        )
        .await;
    assert_eq!(denied.status, 422);
    assert_eq!(denied.error_code(), Some("field_not_permitted"));
    db.cleanup().await;
}

#[tokio::test]
async fn a_site_custom_field_with_a_level_behaves_the_same_before_and_after_a_reload() {
    let binary = plain();
    let db = site(&binary).await;
    // This server polls every 100 ms; a second one never does.
    let fast = db.client_polling(&binary, Duration::from_millis(100));
    let still = db.client_polling(&binary, Duration::from_secs(3600));
    let (agent, manager) = (fast.login("agent").await, fast.login("manager").await);
    let slow_manager = still.login("manager").await;
    assert_eq!(meta_names(&manager).await, ["email", "region"]);

    db.add_custom_field(
        &binary,
        "customer",
        "name = \"custom_limit\"\ntype = \"int\"\nperm_level = 1\n",
    )
    .await;
    // The server that does not poll still has the registry of the apps.
    assert_eq!(meta_names(&slow_manager).await, ["email", "region"]);
    assert!(
        until(Duration::from_secs(10), || async {
            meta_names(&manager)
                .await
                .contains(&"custom_limit".to_owned())
        })
        .await,
        "the polling server never saw the field"
    );
    still.reload_metadata().await;
    assert_eq!(
        meta_names(&slow_manager).await,
        ["email", "region", "custom_limit"]
    );

    // Hidden for the agent, readable and writable for the manager.
    assert_eq!(meta_names(&agent).await, ["email", "region"]);
    let created = manager
        .post(
            "/api/v1/resource/customer",
            json!({"email": "a@x.test", "custom_limit": 40}),
        )
        .await;
    assert_eq!(created.status, 201, "{:?}", created.body);
    let name = created.data()["name"].as_str().expect("name").to_owned();
    let got = agent
        .get(&format!("/api/v1/resource/customer/{name}"))
        .await;
    assert!(!keys(got.data()).contains(&"custom_limit".to_owned()));
    let refused = agent
        .post(
            "/api/v1/resource/customer",
            json!({"email": "b@x.test", "custom_limit": 1}),
        )
        .await;
    assert_eq!(refused.status, 422);
    assert_eq!(
        refused.body["error"]["details"],
        json!(["unknown field `custom_limit`"])
    );
    let filters =
        "%5B%7B%22field%22%3A%22custom_limit%22%2C%22op%22%3A%22%3D%22%2C%22value%22%3A40%7D%5D";
    let list = agent
        .get(&format!("/api/v1/resource/customer?filters={filters}"))
        .await;
    assert_eq!(list.status, 400, "{:?}", list.body);
    assert_eq!(list.error_code(), Some("invalid_query"));
    let list = manager
        .get(&format!("/api/v1/resource/customer?filters={filters}"))
        .await;
    assert_eq!(list.status, 200, "{:?}", list.body);
    assert_eq!(list.data().as_array().expect("list").len(), 1);

    // Disabled: hidden for everybody, administrator included. Enabled: back, with its data.
    db.set_custom_field_enabled("customer", "custom_limit", false)
        .await;
    fast.reload_metadata().await;
    let root = fast.login("root").await;
    assert_eq!(meta_names(&root).await, ["email", "region"]);
    db.set_custom_field_enabled("customer", "custom_limit", true)
        .await;
    fast.reload_metadata().await;
    assert_eq!(meta_names(&root).await, ["email", "region", "custom_limit"]);
    let all = root.get(&format!("/api/v1/resource/customer/{name}")).await;
    assert_eq!(all.data()["custom_limit"], 40);
    db.cleanup().await;
}

#[tokio::test]
async fn the_custom_field_command_and_api_accept_a_level_and_refuse_a_bad_one() {
    let binary = plain();
    let db = site(&binary).await;
    let server = db.client(&binary);
    let root = server.login("root").await;
    let created = root
        .post(
            "/api/v1/customize/customer/fields",
            json!({"name": "custom_a", "type": "int", "perm_level": 2}),
        )
        .await;
    assert_eq!(created.status, 201, "{:?}", created.body);
    let bad = root
        .post(
            "/api/v1/customize/customer/fields",
            json!({"name": "custom_b", "type": "int", "perm_level": 10}),
        )
        .await;
    assert_eq!(bad.status, 422, "{:?}", bad.body);
    // `required` stays refused with a level.
    let required = root
        .post(
            "/api/v1/customize/customer/fields",
            json!({"name": "custom_c", "type": "int", "perm_level": 1, "required": true}),
        )
        .await;
    assert_eq!(required.status, 422, "{:?}", required.body);
    // Level 2 has no rule on this DocType: only the administrator sees the field.
    let manager = server.login("manager").await;
    let fields = meta_names(&manager).await;
    assert!(!fields.contains(&"custom_a".to_owned()), "{fields:?}");
    let listed = root.get("/api/v1/customize/customer/fields").await;
    assert_eq!(
        listed.data()[0]["definition"]["perm_level"],
        2,
        "{:?}",
        listed.body
    );
    db.cleanup().await;
}

#[tokio::test]
async fn a_method_reads_documents_as_its_caller() {
    let binary = binary();
    let db = site(&binary).await;
    let server = db.client(&binary);
    let (agent, manager) = (server.login("agent").await, server.login("manager").await);
    let created = manager
        .post(
            "/api/v1/resource/customer",
            json!({"email": "a@x.test", "credit_limit": 5}),
        )
        .await;
    let name = created.data()["name"].as_str().expect("name").to_owned();
    let as_agent = agent
        .post("/api/v1/method/crm.peek", json!({"name": name}))
        .await;
    assert_eq!(as_agent.status, 200, "{:?}", as_agent.body);
    let seen = as_agent.data()["keys"].as_array().expect("keys").clone();
    assert!(!seen.contains(&json!("credit_limit")), "{seen:?}");
    assert!(seen.contains(&json!("email")));
    let as_manager = manager
        .post("/api/v1/method/crm.peek", json!({"name": name}))
        .await;
    let seen = as_manager.data()["keys"].as_array().expect("keys").clone();
    assert!(seen.contains(&json!("credit_limit")), "{seen:?}");
    db.cleanup().await;
}
