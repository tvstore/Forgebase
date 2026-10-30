//! Tests of the `sales_extension` app, alone and composed with `sales`. The ones that need
//! PostgreSQL (see README.md) fail, they do not skip, when `FORGEBASE_TEST_DATABASE_URL` is not
//! set.

use std::str::FromStr;

use forgebase::testing::{HttpClient, TestDb, assert_doctypes_listed, json, run_jobs};
use forgebase::{
    Decimal, DocTypeHooks, Document, EnqueuedJobs, Forgebase, HookContext, Operation, Value,
};

use sales_extension::{FollowUp, MIN_ORDER_TOTAL, MinimumOrder, app};

/// The same apps, in the same order, as `main.rs`.
fn binary() -> Forgebase {
    Forgebase::new().app(sales::app()).app(app())
}

fn with_total(total: Option<Value>) -> Document {
    let mut doc = Document::default();
    if let Some(total) = total {
        doc.values.insert("total".to_owned(), total);
    }
    doc
}

fn validate(doc: &mut Document) -> Result<(), forgebase::HookError> {
    let ctx = HookContext::new("sales_order", "ann", Operation::Insert);
    MinimumOrder.validate(&ctx, doc)
}

// --- Without a database -------------------------------------------------------------------------

#[test]
fn every_doctype_file_is_listed() {
    assert_doctypes_listed(&app(), env!("CARGO_MANIFEST_DIR"), "doctypes");
}

#[test]
fn the_apps_fit_together() {
    binary().check().expect("valid apps");
}

#[test]
fn the_app_needs_sales_registered_before_it() {
    // Without `sales` the Link to `customer`, the hook and `depends_on` are all errors.
    assert!(Forgebase::new().app(app()).check().is_err());
    assert!(
        Forgebase::new()
            .app(app())
            .app(sales::app())
            .check()
            .is_err(),
        "apps are never sorted"
    );
}

#[test]
fn a_total_below_the_minimum_is_refused() {
    assert_eq!(MIN_ORDER_TOTAL, Decimal::from(10));
    for total in ["0.00", "5.00", "9.99", "-1.00"] {
        let value = Value::Decimal(Decimal::from_str(total).expect("decimal"));
        let error = validate(&mut with_total(Some(value))).expect_err(total);
        assert_eq!(error.message(), "a sales order must total at least 10.00");
    }
}

#[test]
fn the_minimum_and_more_are_accepted() {
    for total in ["10.00", "10", "250.50"] {
        let value = Value::Decimal(Decimal::from_str(total).expect("decimal"));
        validate(&mut with_total(Some(value))).expect(total);
    }
}

#[test]
fn a_missing_or_odd_total_counts_as_zero() {
    for total in [None, Some(Value::Null), Some(Value::Text("99".to_owned()))] {
        assert!(validate(&mut with_total(total)).is_err());
    }
}

#[test]
fn a_submit_enqueues_the_follow_up_job() {
    let jobs = EnqueuedJobs::new();
    let ctx = HookContext::new("sales_order", "ann", Operation::Submit).with_jobs(&jobs);
    let mut doc = Document {
        name: Some("SO-2026-00001".to_owned()),
        ..Document::default()
    };
    FollowUp.before_submit(&ctx, &mut doc).expect("hook");
    let enqueued = jobs.take();
    assert_eq!(enqueued.len(), 1);
    assert_eq!(enqueued[0].name(), "sales_extension.plan_follow_up_visit");
    assert_eq!(enqueued[0].payload(), &json!({"order": "SO-2026-00001"}));
}

// --- Against PostgreSQL, both apps composed -----------------------------------------------------

struct Env {
    db: TestDb,
    user: HttpClient,
    outsider: HttpClient,
}

async fn env() -> Env {
    let binary = binary();
    let db = TestDb::new().await;
    db.migrate(&binary, false).await.expect("migrate");
    db.create_user("ann", &["Sales User"]).await;
    db.create_user("max", &["Sales Manager"]).await;
    db.create_user("eve", &["Accountant"]).await;
    let client = db.client(&binary);
    // A customer for the orders and visits (only a Sales Manager creates one).
    let manager = client.login("max").await;
    let reply = manager
        .post(
            "/api/v1/resource/customer",
            json!({"email": "ada@example.com", "full_name": "Ada Lovelace"}),
        )
        .await;
    assert_eq!(reply.status, 201, "{:?}", reply.body);
    Env {
        user: client.login("ann").await,
        outsider: client.login("eve").await,
        db,
    }
}

#[tokio::test]
async fn the_extension_hook_runs_after_the_sales_hook() {
    let env = env().await;
    // 1 x 5.00, but the client claims a `total` of 100.00. The sales hook overwrites it with
    // the computed 5.00. If the extension hook ran before the sales hook, it would see the
    // user's 100.00 and accept the order, after which the sales hook would compute 5.00. A
    // refusal therefore proves the sales hook ran first.
    let small = env
        .user
        .post(
            "/api/v1/resource/sales_order",
            json!({"customer": "ada@example.com", "total": "100.00",
                   "items": [{"description": "Pen", "qty": 1, "rate": "5.00"}]}),
        )
        .await;
    assert_eq!(small.status, 422, "{:?}", small.body);
    assert_eq!(small.error_code(), Some("hook_rejected"));
    assert!(
        small
            .error_message()
            .expect("message")
            .contains("a sales order must total at least 10.00"),
        "{:?}",
        small.body
    );

    // Even an order without items is refused: its computed total is 0.
    let empty = env
        .user
        .post(
            "/api/v1/resource/sales_order",
            json!({"customer": "ada@example.com"}),
        )
        .await;
    assert_eq!(empty.status, 422, "{:?}", empty.body);

    // 2 x 5.00 = 10.00 is accepted, and keeps the sales rules (computed amount, submit).
    let fine = env
        .user
        .post(
            "/api/v1/resource/sales_order",
            json!({"customer": "ada@example.com",
                   "items": [{"description": "Pen", "qty": 2, "rate": "5.00"}]}),
        )
        .await;
    assert_eq!(fine.status, 201, "{:?}", fine.body);
    assert_eq!(fine.data()["total"], "10.00");
    let name = fine.data()["name"].as_str().expect("name").to_owned();
    let submitted = env
        .user
        .post(
            &format!("/api/v1/resource/sales_order/{name}/submit"),
            json!({"version": 1}),
        )
        .await;
    assert_eq!(submitted.status, 200, "{:?}", submitted.body);
    env.db.cleanup().await;
}

#[tokio::test]
async fn an_update_below_the_minimum_is_refused_too() {
    let env = env().await;
    let created = env
        .user
        .post(
            "/api/v1/resource/sales_order",
            json!({"customer": "ada@example.com",
                   "items": [{"description": "Pen", "qty": 2, "rate": "5.00"}]}),
        )
        .await;
    let name = created.data()["name"].as_str().expect("name").to_owned();
    let refused = env
        .user
        .patch(
            &format!("/api/v1/resource/sales_order/{name}"),
            json!({"version": 1, "data": {"items": [
                {"description": "Pen", "qty": 1, "rate": "5.00"}
            ]}}),
        )
        .await;
    assert_eq!(refused.status, 422, "{:?}", refused.body);
    env.db.cleanup().await;
}

#[tokio::test]
async fn a_visit_links_to_a_customer_of_the_sales_app() {
    let env = env().await;
    let visit = env
        .user
        .post(
            "/api/v1/resource/customer_visit",
            json!({"customer": "ada@example.com", "visited_on": "2026-10-29",
                   "notes": "Showed the new catalogue"}),
        )
        .await;
    assert_eq!(visit.status, 201, "{:?}", visit.body);
    assert_eq!(visit.data()["customer"], "ada@example.com");

    // The framework's Link error names the field.
    let missing = env
        .user
        .post(
            "/api/v1/resource/customer_visit",
            json!({"customer": "nobody@example.com", "visited_on": "2026-10-29"}),
        )
        .await;
    assert_eq!(missing.status, 422, "{:?}", missing.body);
    assert_eq!(missing.error_code(), Some("validation_failed"));
    assert!(
        missing.body["error"]["details"]
            .to_string()
            .contains("customer"),
        "{:?}",
        missing.body
    );
    env.db.cleanup().await;
}

#[tokio::test]
async fn the_metadata_names_the_app_of_every_doctype() {
    let env = env().await;
    let meta = env.user.get("/api/v1/meta").await;
    assert_eq!(meta.status, 200, "{:?}", meta.body);
    let app_of = |name: &str| {
        meta.data()
            .as_array()
            .expect("a list")
            .iter()
            .find(|d| d["name"] == name)
            .unwrap_or_else(|| panic!("{name} is not listed: {:?}", meta.body))["app"]
            .clone()
    };
    assert_eq!(app_of("customer_visit"), "sales_extension");
    assert_eq!(app_of("sales_order"), "sales");
    assert_eq!(app_of("customer"), "sales");
    env.db.cleanup().await;
}

#[tokio::test]
async fn a_user_without_a_sales_role_cannot_create_a_visit() {
    let env = env().await;
    let denied = env
        .outsider
        .post(
            "/api/v1/resource/customer_visit",
            json!({"customer": "ada@example.com", "visited_on": "2026-10-29"}),
        )
        .await;
    assert_eq!(denied.status, 403, "{:?}", denied.body);
    assert_eq!(denied.error_code(), Some("permission_denied"));
    env.db.cleanup().await;
}

// --- Jobs, both apps composed -------------------------------------------------------------------

const ORDERS: &str = "/api/v1/resource/sales_order";

/// A draft of 2 x 5.00 (the smallest total `MinimumOrder` accepts) with an optional
/// `order_date`; returns its name.
async fn draft(env: &Env, order_date: Option<&str>) -> String {
    let mut body = json!({"customer": "ada@example.com",
                          "items": [{"description": "Pen", "qty": 2, "rate": "5.00"}]});
    if let Some(date) = order_date {
        body["order_date"] = json!(date);
    }
    let created = env.user.post(ORDERS, body).await;
    assert_eq!(created.status, 201, "{:?}", created.body);
    created.data()["name"].as_str().expect("name").to_owned()
}

async fn submit(env: &Env, name: &str) -> forgebase::testing::Reply {
    env.user
        .post(&format!("{ORDERS}/{name}/submit"), json!({"version": 1}))
        .await
}

async fn job_names(db: &TestDb) -> Vec<String> {
    db.rows("SELECT name FROM fb_job ORDER BY name")
        .await
        .into_iter()
        .flatten()
        .collect()
}

#[tokio::test]
async fn a_submitted_order_gets_a_follow_up_visit_and_a_log() {
    let env = env().await;
    let name = draft(&env, Some("2026-10-20")).await;
    let submitted = submit(&env, &name).await;
    assert_eq!(submitted.status, 200, "{:?}", submitted.body);
    // One job of each app, both written by the submit.
    assert_eq!(
        job_names(&env.db).await,
        [
            "sales.log_order_event",
            "sales_extension.plan_follow_up_visit"
        ]
    );

    let run = run_jobs(&binary(), &env.db).await;
    assert_eq!((run.done(), run.retried(), run.dead()), (2, 0, 0));

    // The visit is a week after the order date and says which order it follows up.
    let visits = env
        .db
        .rows("SELECT visited_on::text, notes, source_order, owner FROM doc_customer_visit")
        .await;
    assert_eq!(
        visits,
        [[
            "2026-10-27",
            format!("Follow-up of {name}").as_str(),
            name.as_str(),
            "job:sales_extension.plan_follow_up_visit"
        ]]
    );
    let listed = env.user.get("/api/v1/resource/customer_visit").await;
    assert_eq!(listed.status, 200, "{:?}", listed.body);
    assert_eq!(listed.data()[0]["customer"], "ada@example.com");

    // The job of `sales` ran as well: both apps' jobs are in the composed binary.
    let logs = env
        .db
        .rows("SELECT name, owner FROM doc_sales_order_log")
        .await;
    assert_eq!(
        logs,
        [[
            format!("{name}-submitted").as_str(),
            "job:sales.log_order_event"
        ]]
    );
    env.db.cleanup().await;
}

#[tokio::test]
async fn a_job_delivered_twice_plans_one_visit() {
    let env = env().await;
    let name = draft(&env, Some("2026-10-20")).await;
    assert_eq!(submit(&env, &name).await.status, 200);
    env.db
        .exec(
            "INSERT INTO fb_job (id, name, payload, status, enqueued_by)
             SELECT gen_random_uuid(), name, payload, 'queued', 'test' FROM fb_job
             WHERE name = 'sales_extension.plan_follow_up_visit'",
        )
        .await;

    let run = run_jobs(&binary(), &env.db).await;
    assert_eq!((run.done(), run.retried(), run.dead()), (3, 0, 0));
    let visits = env
        .db
        .rows("SELECT count(*)::text FROM doc_customer_visit")
        .await;
    assert_eq!(visits, [["1"]]);
    env.db.cleanup().await;
}

#[tokio::test]
async fn an_order_without_a_date_gets_no_visit() {
    let env = env().await;
    let name = draft(&env, None).await;
    assert_eq!(submit(&env, &name).await.status, 200);
    let run = run_jobs(&binary(), &env.db).await;
    assert_eq!((run.done(), run.dead()), (2, 0));
    let visits = env
        .db
        .rows("SELECT count(*)::text FROM doc_customer_visit")
        .await;
    assert_eq!(visits, [["0"]]);
    env.db.cleanup().await;
}

#[tokio::test]
async fn a_refused_submit_leaves_no_job_of_either_app() {
    let env = env().await;
    // Every save needs a total of 10.00, so a small order cannot be saved. Make the stored
    // order small behind the API's back (a row edited by hand, as after a bad import): the
    // submit recomputes the total from the items, and `MinimumOrder` refuses it.
    let name = draft(&env, Some("2026-10-20")).await;
    env.db.exec("UPDATE doc_sales_order_item SET qty = 1").await;
    let refused = submit(&env, &name).await;
    assert_eq!(refused.status, 422, "{:?}", refused.body);
    assert!(
        refused
            .error_message()
            .expect("message")
            .contains("a sales order must total at least 10.00"),
        "{:?}",
        refused.body
    );
    assert!(job_names(&env.db).await.is_empty());
    env.db.cleanup().await;
}
