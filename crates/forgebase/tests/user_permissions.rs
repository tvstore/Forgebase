//! User permissions end to end (M10.3, ADR 0025), over HTTP with `forgebase::testing`: the
//! Administrator-only API (add, list, remove, validation, the limit, CSRF), that a change applies
//! on the user's next request, that a method reads as its caller, that two sites keep their own
//! rows, and that a restricted user's requests (get, list, summary, export, a write that links to
//! a hidden target) see only what the rows allow.
//!
//! Every test gets its own throwaway database and fails loudly (never skips) when
//! `FORGEBASE_TEST_DATABASE_URL` is not set.

use forgebase::serde_json::{Value as Json, json};
use forgebase::testing::{HttpClient, TestDb};
use forgebase::{
    App, DocTypeFile, Forgebase, ListQuery, Method, MethodContext, MethodFuture, MethodHandler,
};

const GROUP: &str = r#"
name = "customer_group"
label = "Customer Group"
naming = { field = "title" }
[[fields]]
name = "title"
type = "data"
required = true
unique = true
[[permissions]]
role = "Sales"
read = true
write = true
create = true
export = true
"#;

const CUSTOMER: &str = r#"
name = "customer"
label = "Customer"
naming = { field = "title" }
[[fields]]
name = "title"
type = "data"
required = true
unique = true
[[fields]]
name = "customer_group"
type = "link"
target = "customer_group"
[[permissions]]
role = "Sales"
read = true
write = true
create = true
delete = true
export = true
"#;

const FILES: &[DocTypeFile] = &[
    DocTypeFile::new("doctypes/customer_group.toml", GROUP),
    DocTypeFile::new("doctypes/customer.toml", CUSTOMER),
];

/// `crm.customers`: the names of the customers the caller can list, in order; `crm.customer`:
/// whether the caller can get the customer named by `title`.
struct Crm(bool);

impl MethodHandler for Crm {
    fn call<'a>(&'a self, ctx: &'a mut MethodContext<'_>) -> MethodFuture<'a> {
        Box::pin(async move {
            if self.0 {
                let query = ListQuery::new().order_by("title", false);
                let found = ctx.documents().list("customer", &query).await?;
                let names: Vec<&str> = found.iter().map(|d| d.name()).collect();
                Ok(json!(names))
            } else {
                let title = ctx.body()["title"].as_str().unwrap_or_default().to_owned();
                let found = ctx.documents().get("customer", &title).await?;
                Ok(json!({"found": found.is_some()}))
            }
        })
    }
}

fn binary() -> Forgebase {
    Forgebase::new().app(
        App::new("crm")
            .doctypes(FILES)
            .method("customers", Method::new(Crm(true)).roles(&["Sales"]))
            .method("customer", Method::new(Crm(false)).roles(&["Sales"])),
    )
}

/// A migrated site with `ann` and `pat` (Sales), `bob` (no role that reads) and `root`
/// (Administrator), two groups and four customers: `c_retail`, `c_whole`, `c_none` and
/// `c_both` (the group `Retail`, no group, and so on, see `seed`).
async fn site(binary: &Forgebase) -> TestDb {
    let db = TestDb::new().await;
    db.migrate(binary, false).await.expect("migrate");
    for (user, role) in [
        ("ann", "Sales"),
        ("pat", "Sales"),
        ("bob", "Stranger"),
        ("root", "Administrator"),
    ] {
        db.create_user(user, &[role]).await;
    }
    let root = db.client(binary).login("root").await;
    for group in ["Retail", "Wholesale"] {
        let reply = root
            .post("/api/v1/resource/customer_group", json!({"title": group}))
            .await;
        assert_eq!(reply.status, 201, "{:?}", reply.body);
    }
    for (title, group) in [
        ("c_retail", Some("Retail")),
        ("c_whole", Some("Wholesale")),
        ("c_none", None),
    ] {
        let mut body = json!({"title": title});
        if let Some(group) = group {
            body["customer_group"] = json!(group);
        }
        let reply = root.post("/api/v1/resource/customer", body).await;
        assert_eq!(reply.status, 201, "{:?}", reply.body);
    }
    db
}

fn names(reply: &forgebase::testing::Reply) -> Vec<String> {
    assert_eq!(reply.status, 200, "{:?}", reply.body);
    let mut names: Vec<String> = reply
        .data()
        .as_array()
        .expect("array")
        .iter()
        .map(|d| d["name"].as_str().expect("name").to_owned())
        .collect();
    names.sort();
    names
}

fn body(user: &str, doctype: &str, value: &str) -> Json {
    json!({"user": user, "doctype": doctype, "value": value})
}

async fn add(client: &HttpClient, user: &str, doctype: &str, value: &str) -> u16 {
    client
        .post("/api/v1/user-permissions", body(user, doctype, value))
        .await
        .status
}

#[tokio::test]
async fn the_api_is_for_administrators_and_needs_the_csrf_token() {
    let binary = binary();
    let db = site(&binary).await;
    let server = db.client(&binary);
    let root = server.login("root").await;

    // A non-administrator, with the rights on the DocType or not, gets 403 on every route; the
    // cookie of an administrator without the CSRF header gets 403 `csrf_failed`; no credentials 401.
    for user in ["ann", "bob"] {
        let client = server.login(user).await;
        let list = client.get("/api/v1/user-permissions").await;
        assert_eq!(
            (list.status, list.error_code()),
            (403, Some("permission_denied")),
            "{user}"
        );
        let add = client
            .post(
                "/api/v1/user-permissions",
                body("ann", "customer_group", "Retail"),
            )
            .await;
        assert_eq!(
            (add.status, add.error_code()),
            (403, Some("permission_denied"))
        );
        let remove = client
            .post(
                "/api/v1/user-permissions/remove",
                body("ann", "customer_group", "Retail"),
            )
            .await;
        assert_eq!(remove.status, 403, "{user}");
    }
    let reply = root
        .without_csrf()
        .post(
            "/api/v1/user-permissions",
            body("ann", "customer_group", "Retail"),
        )
        .await;
    assert_eq!(
        (reply.status, reply.error_code()),
        (403, Some("csrf_failed"))
    );
    let reply = root
        .without_csrf()
        .post(
            "/api/v1/user-permissions/remove",
            body("ann", "customer_group", "Retail"),
        )
        .await;
    assert_eq!(
        (reply.status, reply.error_code()),
        (403, Some("csrf_failed"))
    );
    assert_eq!(
        server.get("/api/v1/user-permissions").await.status,
        401,
        "no credentials"
    );
    // None of that changed anything.
    assert!(
        root.get("/api/v1/user-permissions")
            .await
            .data()
            .as_array()
            .expect("array")
            .is_empty()
    );

    // An API token of an administrator works like a session, without CSRF.
    let token = db.create_token("root").await;
    let by_token = db.client(&binary).with_token(&token);
    assert_eq!(add(&by_token, "ann", "customer_group", "Retail").await, 201);
}

#[tokio::test]
async fn add_list_and_remove_with_their_validation() {
    let binary = binary();
    let db = site(&binary).await;
    let root = db.client(&binary).login("root").await;

    let created = root
        .post(
            "/api/v1/user-permissions",
            body("ann", "customer_group", "Retail"),
        )
        .await;
    assert_eq!(created.status, 201, "{:?}", created.body);
    assert_eq!(
        created.data(),
        &json!({"user": "ann", "doctype": "customer_group", "value": "Retail", "added": true})
    );
    // Adding it again succeeds and says it was there.
    let again = root
        .post(
            "/api/v1/user-permissions",
            body("ann", "customer_group", "Retail"),
        )
        .await;
    assert_eq!(again.status, 200, "{:?}", again.body);
    assert_eq!(again.data()["added"], false);
    assert_eq!(add(&root, "ann", "customer", "c_none").await, 201);
    assert_eq!(add(&root, "pat", "customer_group", "Wholesale").await, 201);

    // The list: sorted, and filtered by user and DocType.
    let rows = |reply: forgebase::testing::Reply| -> Vec<String> {
        assert_eq!(reply.status, 200, "{:?}", reply.body);
        reply
            .data()
            .as_array()
            .expect("array")
            .iter()
            .map(|r| {
                format!(
                    "{} {} {}",
                    r["user"].as_str().unwrap(),
                    r["doctype"].as_str().unwrap(),
                    r["value"].as_str().unwrap()
                )
            })
            .collect()
    };
    assert_eq!(
        rows(root.get("/api/v1/user-permissions").await),
        [
            "ann customer c_none",
            "ann customer_group Retail",
            "pat customer_group Wholesale"
        ]
    );
    assert_eq!(
        rows(
            root.get("/api/v1/user-permissions?user=ann&doctype=customer_group")
                .await
        ),
        ["ann customer_group Retail"]
    );
    assert_eq!(
        rows(root.get("/api/v1/user-permissions?doctype=customer").await),
        ["ann customer c_none"]
    );
    let first = root.get("/api/v1/user-permissions?user=pat").await;
    assert!(
        first.data()[0]["created_at"]
            .as_str()
            .unwrap()
            .ends_with('Z')
    );
    let unknown_user = root.get("/api/v1/user-permissions?user=nobody").await;
    assert_eq!(unknown_user.status, 404);
    let unknown_param = root.get("/api/v1/user-permissions?bogus=1").await;
    assert_eq!(unknown_param.status, 400, "{:?}", unknown_param.body);

    // Validation, with nothing written.
    for (case, user, doctype, value, status) in [
        ("unknown user", "nobody", "customer_group", "Retail", 404),
        ("bad username", "bad name", "customer_group", "Retail", 422),
        ("unknown DocType", "ann", "ghost", "x", 422),
        ("missing document", "ann", "customer_group", "Nowhere", 422),
        ("empty value", "ann", "customer_group", "", 422),
    ] {
        let reply = root
            .post("/api/v1/user-permissions", body(user, doctype, value))
            .await;
        assert_eq!(reply.status, status, "{case}: {:?}", reply.body);
    }
    for bad in [
        json!({"user": "ann", "doctype": "customer"}),
        json!({"user": "ann", "doctype": "customer", "value": 3}),
        json!({"user": "ann", "doctype": "customer", "value": "c_none", "extra": 1}),
        json!([]),
    ] {
        let reply = root.post("/api/v1/user-permissions", bad.clone()).await;
        assert_eq!(reply.status, 400, "{bad}: {:?}", reply.body);
    }
    assert_eq!(
        rows(root.get("/api/v1/user-permissions").await).len(),
        3,
        "nothing was written by the refusals"
    );

    // Remove: once true, then false; an unknown user is 404.
    let removed = root
        .post(
            "/api/v1/user-permissions/remove",
            body("ann", "customer", "c_none"),
        )
        .await;
    assert_eq!(removed.status, 200, "{:?}", removed.body);
    assert_eq!(removed.data()["removed"], true);
    let removed = root
        .post(
            "/api/v1/user-permissions/remove",
            body("ann", "customer", "c_none"),
        )
        .await;
    assert_eq!(removed.data()["removed"], false);
    let missing = root
        .post(
            "/api/v1/user-permissions/remove",
            body("nobody", "customer", "c_none"),
        )
        .await;
    assert_eq!(missing.status, 404);
}

#[tokio::test]
async fn a_user_can_hold_at_most_a_thousand_documents_per_doctype() {
    let binary = binary();
    let db = site(&binary).await;
    let root = db.client(&binary).login("root").await;
    db.exec(
        "INSERT INTO fb_user_permission (user_id, doctype, value) \
         SELECT u.id, 'customer_group', 'n' || g FROM fb_user u, generate_series(1, 999) g \
         WHERE u.username = 'ann'",
    )
    .await;
    assert_eq!(add(&root, "ann", "customer_group", "Retail").await, 201);
    let refused = root
        .post(
            "/api/v1/user-permissions",
            body("ann", "customer_group", "Wholesale"),
        )
        .await;
    assert_eq!(refused.status, 422, "{:?}", refused.body);
    assert!(
        refused.body.to_string().contains("1000"),
        "{:?}",
        refused.body
    );
    // The same row again and another DocType are fine.
    assert_eq!(add(&root, "ann", "customer_group", "Retail").await, 200);
    assert_eq!(add(&root, "ann", "customer", "c_none").await, 201);
}

#[tokio::test]
async fn a_change_applies_on_the_users_next_request() {
    let binary = binary();
    let db = site(&binary).await;
    let server = db.client(&binary);
    let root = server.login("root").await;
    // One session of Ann, kept open across every change below.
    let ann = server.login("ann").await;
    let list = |client: HttpClient| async move {
        names(&client.get("/api/v1/resource/customer?limit=100").await)
    };
    assert_eq!(list(ann.clone()).await, ["c_none", "c_retail", "c_whole"]);

    assert_eq!(add(&root, "ann", "customer_group", "Retail").await, 201);
    assert_eq!(list(ann.clone()).await, ["c_none", "c_retail"]);
    // The documents she cannot see are 404, exactly as a missing one.
    let hidden = ann.get("/api/v1/resource/customer/c_whole").await;
    let missing = ann.get("/api/v1/resource/customer/c_nowhere").await;
    assert_eq!((hidden.status, missing.status), (404, 404));
    assert_eq!(hidden.body["error"]["code"], missing.body["error"]["code"]);
    assert_eq!(
        ann.get("/api/v1/resource/customer/c_retail").await.status,
        200
    );

    // Allowing a second group, then none: the next request sees it, with no reload.
    assert_eq!(add(&root, "ann", "customer_group", "Wholesale").await, 201);
    assert_eq!(list(ann.clone()).await, ["c_none", "c_retail", "c_whole"]);
    for group in ["Retail", "Wholesale"] {
        let reply = root
            .post(
                "/api/v1/user-permissions/remove",
                body("ann", "customer_group", group),
            )
            .await;
        assert_eq!(reply.status, 200);
    }
    assert_eq!(list(ann.clone()).await, ["c_none", "c_retail", "c_whole"]);
    // The same through an API token.
    let token = db.create_token("ann").await;
    let by_token = server.with_token(&token);
    assert_eq!(add(&root, "ann", "customer_group", "Retail").await, 201);
    assert_eq!(list(by_token).await, ["c_none", "c_retail"]);
}

#[tokio::test]
async fn a_restricted_user_sees_the_allowed_documents_on_every_route() {
    let binary = binary();
    let db = site(&binary).await;
    let server = db.client(&binary);
    let root = server.login("root").await;
    assert_eq!(add(&root, "ann", "customer_group", "Retail").await, 201);
    let ann = server.login("ann").await;
    let pat = server.login("pat").await;

    // The restricted DocType, list and get.
    assert_eq!(
        names(&ann.get("/api/v1/resource/customer_group").await),
        ["Retail"]
    );
    assert_eq!(
        names(&pat.get("/api/v1/resource/customer_group").await),
        ["Retail", "Wholesale"]
    );
    assert_eq!(
        ann.get("/api/v1/resource/customer_group/Wholesale")
            .await
            .status,
        404
    );
    // The group-by summary counts only visible customers.
    let summary = ann
        .get("/api/v1/summary/customer?group_by=customer_group")
        .await;
    assert_eq!(summary.status, 200, "{:?}", summary.body);
    let total: i64 = summary.data()["groups"]
        .as_array()
        .expect("groups")
        .iter()
        .map(|g| g["count"].as_i64().unwrap_or(0))
        .sum();
    assert_eq!(total, 2, "{:?}", summary.body);
    // The export carries only the visible rows.
    let csv = ann.get("/api/v1/export/customer?format=json").await;
    assert_eq!(csv.status, 200, "{:?}", csv.body);
    let exported: Vec<&str> = csv
        .body
        .as_array()
        .map(|rows| rows.iter().filter_map(|r| r["name"].as_str()).collect())
        .unwrap_or_default();
    assert!(
        !exported.contains(&"c_whole") && exported.contains(&"c_retail"),
        "{:?}",
        csv.body
    );

    // A write that links to a hidden target is a missing target, the same as a name that does
    // not exist; creating in the restricted DocType is refused.
    let hidden = ann
        .post(
            "/api/v1/resource/customer",
            json!({"title": "n1", "customer_group": "Wholesale"}),
        )
        .await;
    let missing = ann
        .post(
            "/api/v1/resource/customer",
            json!({"title": "n2", "customer_group": "Nowhere"}),
        )
        .await;
    assert_eq!(
        (hidden.status, missing.status),
        (422, 422),
        "{:?}",
        hidden.body
    );
    assert_eq!(
        hidden.body.to_string().replace("Wholesale", "N"),
        missing.body.to_string().replace("Nowhere", "N")
    );
    let ok = ann
        .post(
            "/api/v1/resource/customer",
            json!({"title": "n3", "customer_group": "Retail"}),
        )
        .await;
    assert_eq!(ok.status, 201, "{:?}", ok.body);
    let create = ann
        .post(
            "/api/v1/resource/customer_group",
            json!({"title": "Online"}),
        )
        .await;
    assert_eq!(
        (create.status, create.error_code()),
        (403, Some("permission_denied")),
        "{:?}",
        create.body
    );
    // The Administrator is not restricted whatever rows there are.
    assert_eq!(add(&root, "root", "customer_group", "Retail").await, 201);
    let root = server.login("root").await;
    assert_eq!(
        names(&root.get("/api/v1/resource/customer_group").await),
        ["Retail", "Wholesale"]
    );
}

#[tokio::test]
async fn a_method_reads_as_the_caller_and_inherits_the_restriction() {
    let binary = binary();
    let db = site(&binary).await;
    let server = db.client(&binary);
    let root = server.login("root").await;
    assert_eq!(add(&root, "ann", "customer_group", "Retail").await, 201);
    let ann = server.login("ann").await;
    let pat = server.login("pat").await;

    let customers = |client: HttpClient| async move {
        let reply = client.post("/api/v1/method/crm.customers", json!({})).await;
        assert_eq!(reply.status, 200, "{:?}", reply.body);
        reply.data().clone()
    };
    assert_eq!(customers(ann.clone()).await, json!(["c_none", "c_retail"]));
    assert_eq!(
        customers(pat.clone()).await,
        json!(["c_none", "c_retail", "c_whole"])
    );
    let found = |client: HttpClient, title: &'static str| async move {
        client
            .post("/api/v1/method/crm.customer", json!({"title": title}))
            .await
            .data()["found"]
            .as_bool()
    };
    assert_eq!(found(ann.clone(), "c_retail").await, Some(true));
    assert_eq!(found(ann.clone(), "c_whole").await, Some(false));
    assert_eq!(found(pat.clone(), "c_whole").await, Some(true));
}

#[tokio::test]
async fn two_sites_keep_their_own_permissions() {
    let binary = binary();
    let one = site(&binary).await;
    let two = site(&binary).await;
    let root_one = one.client(&binary).login("root").await;
    assert_eq!(add(&root_one, "ann", "customer_group", "Retail").await, 201);
    let ann_one = one.client(&binary).login("ann").await;
    let ann_two = two.client(&binary).login("ann").await;
    assert_eq!(
        names(&ann_one.get("/api/v1/resource/customer?limit=100").await),
        ["c_none", "c_retail"]
    );
    assert_eq!(
        names(&ann_two.get("/api/v1/resource/customer?limit=100").await),
        ["c_none", "c_retail", "c_whole"]
    );
    let root_two = two.client(&binary).login("root").await;
    assert!(
        root_two
            .get("/api/v1/user-permissions")
            .await
            .data()
            .as_array()
            .expect("array")
            .is_empty()
    );
}
