//! Tests of the `sales_extension` app, alone and composed with `sales`. The ones that need
//! PostgreSQL (see README.md) fail, they do not skip, when `FORGEBASE_TEST_DATABASE_URL` is not
//! set.

use std::str::FromStr;

use forgebase::testing::{HttpClient, TestDb, assert_doctypes_listed, json};
use forgebase::{Decimal, DocTypeHooks, Document, Forgebase, HookContext, Operation, Value};

use sales_extension::{MIN_ORDER_TOTAL, MinimumOrder, app};

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
        assert_eq!(error.message(), "a sales order must total at least 10");
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
    // 1 x 5.00: the client sends no `total` and no `amount`, so only the sales hook can have
    // computed the 5.00 that the extension hook rejects.
    let small = env
        .user
        .post(
            "/api/v1/resource/sales_order",
            json!({"customer": "ada@example.com",
                   "items": [{"description": "Pen", "qty": 1, "rate": "5.00"}]}),
        )
        .await;
    assert_eq!(small.status, 422, "{:?}", small.body);
    assert_eq!(small.error_code(), Some("hook_rejected"));
    assert!(
        small
            .error_message()
            .expect("message")
            .contains("a sales order must total at least 10"),
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
