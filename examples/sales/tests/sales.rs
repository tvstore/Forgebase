//! Tests of the `sales` app. The ones that need PostgreSQL (see README.md) fail, they do not
//! skip, when `FORGEBASE_TEST_DATABASE_URL` is not set.

use std::str::FromStr;

use forgebase::testing::{HttpClient, TestDb, assert_doctypes_listed, json};
use forgebase::{Decimal, DocTypeHooks, Document, Forgebase, HookContext, Operation, Row, Value};

use sales::{OrderRules, app};

const ORDERS: &str = "/api/v1/resource/sales_order";

fn dec(text: &str) -> Decimal {
    Decimal::from_str(text).expect("a decimal")
}

fn item(qty: Value, rate: Value) -> Row {
    Row::from([("qty".to_owned(), qty), ("rate".to_owned(), rate)])
}

fn order(items: Vec<Row>) -> Document {
    let mut doc = Document::default();
    doc.tables.insert("items".to_owned(), items);
    doc
}

fn validate(doc: &mut Document) {
    let ctx = HookContext::new("sales_order", "ann", Operation::Insert);
    OrderRules.validate(&ctx, doc).expect("validate");
}

fn total(doc: &Document) -> Decimal {
    match doc.values.get("total") {
        Some(Value::Decimal(total)) => *total,
        other => panic!("no total: {other:?}"),
    }
}

fn amount(doc: &Document, index: usize) -> Value {
    doc.tables["items"][index]["amount"].clone()
}

// --- Without a database -------------------------------------------------------------------------

#[test]
fn every_doctype_file_is_listed() {
    assert_doctypes_listed(&app(), env!("CARGO_MANIFEST_DIR"), "doctypes");
}

#[test]
fn the_app_fits_together() {
    Forgebase::new().app(app()).check().expect("valid app");
}

#[test]
fn amounts_and_total_are_computed() {
    let mut doc = order(vec![
        item(Value::Int(3), Value::Decimal(dec("10.50"))),
        item(Value::Int(2), Value::Decimal(dec("0.25"))),
    ]);
    validate(&mut doc);
    assert_eq!(amount(&doc, 0), Value::Decimal(dec("31.50")));
    assert_eq!(amount(&doc, 1), Value::Decimal(dec("0.50")));
    assert_eq!(total(&doc), dec("32.00"));
}

#[test]
fn computed_values_overwrite_what_the_client_sent() {
    let mut row = item(Value::Int(1), Value::Decimal(dec("5.00")));
    row.insert("amount".to_owned(), Value::Decimal(dec("999.00")));
    let mut doc = order(vec![row]);
    doc.values
        .insert("total".to_owned(), Value::Decimal(dec("999.00")));
    validate(&mut doc);
    assert_eq!(amount(&doc, 0), Value::Decimal(dec("5.00")));
    assert_eq!(total(&doc), dec("5.00"));
}

#[test]
fn amounts_round_half_away_from_zero_to_two_places() {
    // 3 * 0.005 = 0.015 -> 0.02 (half to even would give 0.02 too), 1 * 0.125 -> 0.13 (even: 0.12),
    // -1 * 0.125 -> -0.13, 1 * 2.344 -> 2.34.
    let mut doc = order(vec![
        item(Value::Int(3), Value::Decimal(dec("0.005"))),
        item(Value::Int(1), Value::Decimal(dec("0.125"))),
        item(Value::Int(-1), Value::Decimal(dec("0.125"))),
        item(Value::Int(1), Value::Decimal(dec("2.344"))),
    ]);
    validate(&mut doc);
    assert_eq!(amount(&doc, 0), Value::Decimal(dec("0.02")));
    assert_eq!(amount(&doc, 1), Value::Decimal(dec("0.13")));
    assert_eq!(amount(&doc, 2), Value::Decimal(dec("-0.13")));
    assert_eq!(amount(&doc, 3), Value::Decimal(dec("2.34")));
    assert_eq!(total(&doc), dec("2.36"));
}

#[test]
fn missing_or_odd_values_are_left_for_the_framework() {
    let mut doc = order(vec![
        item(Value::Null, Value::Decimal(dec("1.00"))),
        item(Value::Int(1), Value::Text("cheap".to_owned())),
        Row::new(),
        item(Value::Int(2), Value::Decimal(dec("4.00"))),
    ]);
    validate(&mut doc);
    for index in 0..3 {
        assert_eq!(amount(&doc, index), Value::Null, "row {index}");
    }
    assert_eq!(amount(&doc, 3), Value::Decimal(dec("8.00")));
    assert_eq!(total(&doc), dec("8.00"));
}

#[test]
fn an_order_without_items_totals_zero() {
    let mut doc = Document::default();
    validate(&mut doc);
    assert_eq!(total(&doc), Decimal::ZERO);
}

#[test]
fn an_overflowing_amount_is_an_error_not_a_panic() {
    let mut doc = order(vec![item(
        Value::Int(i64::MAX),
        Value::Decimal(Decimal::MAX),
    )]);
    let ctx = HookContext::new("sales_order", "ann", Operation::Insert);
    assert!(OrderRules.validate(&ctx, &mut doc).is_err());
}

#[test]
fn submit_is_refused_without_items() {
    let ctx = HookContext::new("sales_order", "ann", Operation::Submit);
    for mut doc in [Document::default(), order(vec![])] {
        let error = OrderRules
            .before_submit(&ctx, &mut doc)
            .expect_err("no items");
        assert_eq!(
            error.message(),
            "a sales order needs at least one item to be submitted"
        );
    }
}

#[test]
fn submit_is_refused_when_the_total_is_not_positive() {
    let ctx = HookContext::new("sales_order", "ann", Operation::Submit);
    for total in ["0.00", "-5.00"] {
        let mut doc = order(vec![item(Value::Int(1), Value::Decimal(dec("1.00")))]);
        doc.values
            .insert("total".to_owned(), Value::Decimal(dec(total)));
        assert!(OrderRules.before_submit(&ctx, &mut doc).is_err(), "{total}");
    }
    // No total at all is refused too.
    let mut doc = order(vec![item(Value::Int(1), Value::Decimal(dec("1.00")))]);
    assert!(OrderRules.before_submit(&ctx, &mut doc).is_err());

    doc.values
        .insert("total".to_owned(), Value::Decimal(dec("0.01")));
    OrderRules.before_submit(&ctx, &mut doc).expect("positive");
}

// --- Against PostgreSQL -------------------------------------------------------------------------

struct Env {
    db: TestDb,
    user: HttpClient,
    manager: HttpClient,
    outsider: HttpClient,
}

async fn env() -> Env {
    let binary = Forgebase::new().app(app());
    let db = TestDb::new().await;
    db.migrate(&binary, false).await.expect("migrate");
    db.create_user("ann", &["Sales User"]).await;
    db.create_user("max", &["Sales Manager"]).await;
    db.create_user("eve", &["Accountant"]).await;
    let client = db.client(&binary);
    Env {
        user: client.login("ann").await,
        manager: client.login("max").await,
        outsider: client.login("eve").await,
        db,
    }
}

async fn customer(client: &HttpClient) {
    // Only a Sales Manager creates customers; the caller passes the manager's client.
    let reply = client
        .post(
            "/api/v1/resource/customer",
            json!({"email": "ada@example.com", "full_name": "Ada Lovelace"}),
        )
        .await;
    assert_eq!(reply.status, 201, "{:?}", reply.body);
}

fn two_items() -> forgebase::testing::Json {
    json!([
        {"description": "Widget", "qty": 3, "rate": "10.50"},
        {"description": "Gadget", "qty": 2, "rate": "0.25"}
    ])
}

#[tokio::test]
async fn the_migration_creates_the_groups_once() {
    let binary = Forgebase::new().app(app());
    let db = TestDb::new().await;
    let first = db.migrate(&binary, false).await.expect("migrate");
    assert_eq!(first.app_migrations.len(), 1);
    assert_eq!(
        first.app_migrations[0],
        forgebase::testing::AppliedMigration::new("sales", 1, "default_customer_groups")
    );
    let groups = db
        .rows("SELECT name, title, owner, version::text FROM doc_customer_group ORDER BY name")
        .await;
    assert_eq!(
        groups,
        [
            ["Retail", "Retail", "system", "1"],
            ["Wholesale", "Wholesale", "system", "1"]
        ]
    );

    // A second migrate is a no-op, and the migration is recorded once.
    let second = db.migrate(&binary, false).await.expect("migrate again");
    assert!(second.app_migrations.is_empty());
    let recorded = db
        .rows("SELECT app, version::text, name FROM fb_app_migration")
        .await;
    assert_eq!(recorded, [["sales", "1", "default_customer_groups"]]);
    let count = db
        .rows("SELECT count(*)::text FROM doc_customer_group")
        .await;
    assert_eq!(count, [["2"]]);
    db.cleanup().await;
}

#[tokio::test]
async fn the_groups_are_usable_and_the_sql_is_idempotent() {
    let env = env().await;
    // The statement tolerates a group that exists (here: run again by hand).
    let sql = include_str!("../migrations/0001_default_customer_groups.sql");
    env.db.exec(sql).await;
    let count = env
        .db
        .rows("SELECT count(*)::text FROM doc_customer_group")
        .await;
    assert_eq!(count, [["2"]]);

    // A row written by the migration is a real document: a customer can link to it.
    let reply = env
        .manager
        .post(
            "/api/v1/resource/customer",
            json!({"email": "ada@example.com", "full_name": "Ada", "customer_group": "Retail"}),
        )
        .await;
    assert_eq!(reply.status, 201, "{:?}", reply.body);
    assert_eq!(reply.data()["customer_group"], "Retail");
    env.db.cleanup().await;
}

#[tokio::test]
async fn an_order_gets_its_amounts_and_total_and_an_update_recomputes_them() {
    let env = env().await;
    customer(&env.manager).await;

    // The client sends no amounts, and a wrong total: both are computed.
    let created = env
        .user
        .post(
            ORDERS,
            json!({"customer": "ada@example.com", "items": two_items(), "total": "1.00"}),
        )
        .await;
    assert_eq!(created.status, 201, "{:?}", created.body);
    let data = created.data();
    assert_eq!(data["total"], "32.00");
    assert_eq!(data["items"][0]["amount"], "31.50");
    assert_eq!(data["items"][1]["amount"], "0.50");
    let name = data["name"].as_str().expect("name").to_owned();

    // A new quantity changes the amount and the total.
    let updated = env
        .user
        .patch(
            &format!("{ORDERS}/{name}"),
            json!({"version": 1, "data": {"items": [
                {"description": "Widget", "qty": 4, "rate": "10.50"}
            ]}}),
        )
        .await;
    assert_eq!(updated.status, 200, "{:?}", updated.body);
    assert_eq!(updated.data()["total"], "42.00");
    assert_eq!(updated.data()["items"][0]["amount"], "42.00");
    env.db.cleanup().await;
}

#[tokio::test]
async fn a_draft_may_have_no_items_but_cannot_be_submitted() {
    let env = env().await;
    customer(&env.manager).await;
    let created = env
        .user
        .post(ORDERS, json!({"customer": "ada@example.com"}))
        .await;
    assert_eq!(created.status, 201, "{:?}", created.body);
    assert_eq!(created.data()["total"], "0.00");
    let name = created.data()["name"].as_str().expect("name").to_owned();

    // The framework reports a hook error as 422 `hook_rejected`.
    let refused = env
        .user
        .post(&format!("{ORDERS}/{name}/submit"), json!({"version": 1}))
        .await;
    assert_eq!(refused.status, 422, "{:?}", refused.body);
    assert_eq!(refused.error_code(), Some("hook_rejected"));
    assert!(
        refused
            .error_message()
            .expect("message")
            .contains("a sales order needs at least one item to be submitted"),
        "{:?}",
        refused.body
    );
    // Nothing changed: still a draft, same version.
    let read = env.user.get(&format!("{ORDERS}/{name}")).await;
    assert_eq!(read.data()["docstatus"], 0);
    assert_eq!(read.data()["version"], 1);
    env.db.cleanup().await;
}

#[tokio::test]
async fn an_order_with_items_can_be_submitted() {
    let env = env().await;
    customer(&env.manager).await;
    let created = env
        .user
        .post(
            ORDERS,
            json!({"customer": "ada@example.com", "items": two_items()}),
        )
        .await;
    let name = created.data()["name"].as_str().expect("name").to_owned();

    let submitted = env
        .user
        .post(&format!("{ORDERS}/{name}/submit"), json!({"version": 1}))
        .await;
    assert_eq!(submitted.status, 200, "{:?}", submitted.body);
    assert_eq!(submitted.data()["docstatus"], 1);
    assert_eq!(submitted.data()["total"], "32.00");
    env.db.cleanup().await;
}

#[tokio::test]
async fn permissions_are_enforced() {
    let env = env().await;
    customer(&env.manager).await;
    let created = env
        .user
        .post(
            ORDERS,
            json!({"customer": "ada@example.com", "items": two_items()}),
        )
        .await;
    let name = created.data()["name"].as_str().expect("name").to_owned();
    let submitted = env
        .user
        .post(&format!("{ORDERS}/{name}/submit"), json!({"version": 1}))
        .await;
    assert_eq!(submitted.status, 200, "{:?}", submitted.body);

    // A Sales User cannot cancel; a Sales Manager can.
    let cancel = format!("{ORDERS}/{name}/cancel");
    let denied = env.user.post(&cancel, json!({"version": 2})).await;
    assert_eq!(denied.status, 403, "{:?}", denied.body);
    assert_eq!(denied.error_code(), Some("permission_denied"));
    let cancelled = env.manager.post(&cancel, json!({"version": 2})).await;
    assert_eq!(cancelled.status, 200, "{:?}", cancelled.body);
    assert_eq!(cancelled.data()["docstatus"], 2);

    // A user without a sales role cannot read orders, neither one nor the list.
    let one = env.outsider.get(&format!("{ORDERS}/{name}")).await;
    assert_eq!(one.status, 403, "{:?}", one.body);
    let list = env.outsider.get(ORDERS).await;
    assert_eq!(list.status, 403, "{:?}", list.body);
    env.db.cleanup().await;
}
