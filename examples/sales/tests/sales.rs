//! Tests of the `sales` app. The ones that need PostgreSQL (see README.md) fail, they do not
//! skip, when `FORGEBASE_TEST_DATABASE_URL` is not set.

use std::str::FromStr;

use forgebase::testing::{
    HttpClient, TestDb, assert_doctypes_listed, check_reports, json, run_jobs, run_schedules_at,
};
use forgebase::{
    Decimal, DocTypeHooks, Document, EnqueuedJobs, Forgebase, HookContext, OffsetDateTime,
    Operation, Row, TransitionInfo, Value,
};

use sales::{APPROVAL_LIMIT, OrderRules, app};

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
fn computed_values_overwrite_what_the_document_holds() {
    // The hook alone: a document that already holds values (a client cannot send them, they are
    // read_only, but a copy such as an amendment can carry them) gets them recomputed.
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

    // A positive total passes. The hook also enqueues a job, so it needs a collector and a name.
    doc.values
        .insert("total".to_owned(), Value::Decimal(dec("0.01")));
    doc.name = Some("SO-2026-00001".to_owned());
    let jobs = EnqueuedJobs::new();
    let ctx = ctx.with_jobs(&jobs);
    OrderRules.before_submit(&ctx, &mut doc).expect("positive");
}

#[test]
fn submit_and_cancel_enqueue_the_log_job() {
    let jobs = EnqueuedJobs::new();
    let mut doc = order(vec![item(Value::Int(1), Value::Decimal(dec("1.00")))]);
    doc.name = Some("SO-2026-00001".to_owned());
    validate(&mut doc);

    let submit = HookContext::new("sales_order", "ann", Operation::Submit).with_jobs(&jobs);
    OrderRules.before_submit(&submit, &mut doc).expect("submit");
    let cancel = HookContext::new("sales_order", "max", Operation::Cancel).with_jobs(&jobs);
    OrderRules.before_cancel(&cancel, &doc).expect("cancel");

    let enqueued = jobs.take();
    assert_eq!(enqueued.len(), 2);
    for (job, event) in enqueued.iter().zip(["submitted", "cancelled"]) {
        assert_eq!(job.name(), "sales.log_order_event");
        assert_eq!(
            job.payload(),
            &json!({"order": "SO-2026-00001", "event": event})
        );
    }
}

#[test]
fn a_refused_submit_enqueues_nothing() {
    let jobs = EnqueuedJobs::new();
    let ctx = HookContext::new("sales_order", "ann", Operation::Submit).with_jobs(&jobs);
    let mut doc = order(vec![]);
    doc.name = Some("SO-2026-00001".to_owned());
    assert!(OrderRules.before_submit(&ctx, &mut doc).is_err());
    assert!(jobs.take().is_empty());
}

fn approve_hook(action: &str, total: &str) -> Result<(), forgebase::HookError> {
    let info = TransitionInfo::new(action, "Pending Approval", "Approved");
    let ctx = HookContext::new("sales_order", "ann", Operation::Transition).with_transition(&info);
    let mut doc = Document::default();
    doc.values
        .insert("total".to_owned(), Value::Decimal(dec(total)));
    OrderRules.before_transition(&ctx, &mut doc)
}

#[test]
fn approve_is_limited_to_the_approval_limit_and_the_limit_itself_passes() {
    assert_eq!(APPROVAL_LIMIT, dec("10000.00"));
    approve_hook("Approve", "10000.00").expect("the limit itself is allowed");
    let error = approve_hook("Approve", "10000.01").expect_err("above the limit");
    assert_eq!(
        error.message(),
        "orders above 10000.00 need a Sales Manager: use 'Approve as Manager'"
    );
}

#[test]
fn only_the_approve_action_is_limited() {
    for action in [
        "Approve as Manager",
        "Reject",
        "Submit for Approval",
        "Cancel",
    ] {
        approve_hook(action, "99999.00").unwrap_or_else(|e| panic!("{action}: {e:?}"));
    }
}

// --- Against PostgreSQL -------------------------------------------------------------------------

struct Env {
    db: TestDb,
    /// Not logged in.
    anon: HttpClient,
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
        anon: client,
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

/// Takes the transition `action` on the order `name` (at `version`) as `client`.
async fn transition(
    client: &HttpClient,
    name: &str,
    action: &str,
    version: i64,
) -> forgebase::testing::Reply {
    client
        .post(
            &format!("{ORDERS}/{name}/transition"),
            json!({"action": action, "version": version}),
        )
        .await
}

/// Sends the draft `name` (version 1) for approval and approves it, both as `client`: the
/// order is `Approved` (submitted) at version 3.
async fn approved(client: &HttpClient, name: &str) {
    let sent = transition(client, name, "Submit for Approval", 1).await;
    assert_eq!(sent.status, 200, "{:?}", sent.body);
    let done = transition(client, name, "Approve", 2).await;
    assert_eq!(done.status, 200, "{:?}", done.body);
    assert_eq!(done.data()["workflow_state"], "Approved");
    assert_eq!(done.data()["docstatus"], 1);
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
        .rows("SELECT name, title, owner, version::text FROM doc_customer_group ORDER BY name COLLATE \"C\"")
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

    // A client cannot send a total: it is read_only and refused (422), not silently replaced.
    let refused = env
        .user
        .post(
            ORDERS,
            json!({"customer": "ada@example.com", "items": two_items(), "total": "1.00"}),
        )
        .await;
    assert_eq!(refused.status, 422, "{:?}", refused.body);
    assert_eq!(refused.error_code(), Some("validation_failed"));
    // Nor an amount in a row.
    let mut rows = two_items();
    rows[0]["amount"] = json!("1.00");
    let refused = env
        .user
        .post(
            ORDERS,
            json!({"customer": "ada@example.com", "items": rows}),
        )
        .await;
    assert_eq!(refused.status, 422, "{:?}", refused.body);

    // Without them, the amounts and the total are computed.
    let created = env
        .user
        .post(
            ORDERS,
            json!({"customer": "ada@example.com", "items": two_items()}),
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

    // Sending the order for approval is a draft-to-draft move and needs no items. Approving
    // submits it, and the framework reports the hook error as 422 `hook_rejected`.
    let sent = transition(&env.user, &name, "Submit for Approval", 1).await;
    assert_eq!(sent.status, 200, "{:?}", sent.body);
    let refused = transition(&env.user, &name, "Approve", 2).await;
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
    // Nothing changed: still pending approval and a draft, same version.
    let read = env.user.get(&format!("{ORDERS}/{name}")).await;
    assert_eq!(read.data()["docstatus"], 0);
    assert_eq!(read.data()["workflow_state"], "Pending Approval");
    assert_eq!(read.data()["version"], 2);
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

    approved(&env.user, &name).await;
    let read = env.user.get(&format!("{ORDERS}/{name}")).await;
    assert_eq!(read.data()["docstatus"], 1);
    assert_eq!(read.data()["total"], "32.00");
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
    approved(&env.user, &name).await;

    // A Sales User cannot cancel (no right, no role); a Sales Manager can.
    let denied = transition(&env.user, &name, "Cancel", 3).await;
    assert_eq!(denied.status, 403, "{:?}", denied.body);
    assert_eq!(denied.error_code(), Some("permission_denied"));
    let cancelled = transition(&env.manager, &name, "Cancel", 3).await;
    assert_eq!(cancelled.status, 200, "{:?}", cancelled.body);
    assert_eq!(cancelled.data()["docstatus"], 2);

    // A user without a sales role cannot read orders, neither one nor the list.
    let one = env.outsider.get(&format!("{ORDERS}/{name}")).await;
    assert_eq!(one.status, 403, "{:?}", one.body);
    let list = env.outsider.get(ORDERS).await;
    assert_eq!(list.status, 403, "{:?}", list.body);
    env.db.cleanup().await;
}

// --- The approval workflow ----------------------------------------------------------------------

/// The `fb_workflow_log` rows of an order as "action: from -> to by actor", oldest first.
async fn log_of(db: &TestDb, name: &str) -> Vec<String> {
    db.rows(&format!(
        "SELECT action || ': ' || from_state || ' -> ' || to_state || ' by ' || actor
         FROM fb_workflow_log WHERE doctype = 'sales_order' AND name = '{name}' ORDER BY id"
    ))
    .await
    .into_iter()
    .flatten()
    .collect()
}

fn big_items() -> forgebase::testing::Json {
    // 10 000.01: one cent above `APPROVAL_LIMIT`.
    json!([{"description": "Machine", "qty": 1, "rate": "10000.01"}])
}

#[tokio::test]
async fn an_order_goes_from_draft_to_cancelled_through_the_workflow() {
    let env = env().await;
    customer(&env.manager).await;
    let name = draft(&env, None, two_items()).await;
    let read = env.user.get(&format!("{ORDERS}/{name}")).await;
    assert_eq!(read.data()["workflow_state"], "Draft");
    assert_eq!(read.data()["docstatus"], 0);

    // What the Sales User may do now.
    let offered = env.user.get(&format!("{ORDERS}/{name}/transitions")).await;
    assert_eq!(offered.status, 200, "{:?}", offered.body);
    assert_eq!(
        offered.data()["transitions"],
        json!([{"action": "Submit for Approval", "to": "Pending Approval"}])
    );

    let sent = transition(&env.user, &name, "Submit for Approval", 1).await;
    assert_eq!(sent.status, 200, "{:?}", sent.body);
    assert_eq!(sent.data()["workflow_state"], "Pending Approval");
    assert_eq!(sent.data()["docstatus"], 0);
    let approved = transition(&env.user, &name, "Approve", 2).await;
    assert_eq!(approved.status, 200, "{:?}", approved.body);
    assert_eq!(approved.data()["workflow_state"], "Approved");
    assert_eq!(approved.data()["docstatus"], 1);
    let cancelled = transition(&env.manager, &name, "Cancel", 3).await;
    assert_eq!(cancelled.status, 200, "{:?}", cancelled.body);
    assert_eq!(cancelled.data()["workflow_state"], "Cancelled");
    assert_eq!(cancelled.data()["docstatus"], 2);

    // Every move left a log row, written with the transition; the API shows the same history.
    assert_eq!(
        log_of(&env.db, &name).await,
        [
            "Submit for Approval: Draft -> Pending Approval by ann",
            "Approve: Pending Approval -> Approved by ann",
            "Cancel: Approved -> Cancelled by max"
        ]
    );
    let history = env.user.get(&format!("{ORDERS}/{name}/history")).await;
    assert_eq!(history.data()["history"].as_array().map(Vec::len), Some(3));

    // The hooks of the submit and the cancel ran inside the transitions: both jobs exist.
    let jobs = env
        .db
        .rows(
            "SELECT payload->>'event' FROM fb_job WHERE name = 'sales.log_order_event' ORDER BY 1",
        )
        .await;
    assert_eq!(jobs, [["cancelled"], ["submitted"]]);
    env.db.cleanup().await;
}

#[tokio::test]
async fn plain_submit_and_cancel_are_refused_on_an_order() {
    let env = env().await;
    customer(&env.manager).await;
    let name = draft(&env, None, two_items()).await;
    let refused = env
        .user
        .post(&format!("{ORDERS}/{name}/submit"), json!({"version": 1}))
        .await;
    assert_eq!(refused.status, 409, "{:?}", refused.body);
    assert_eq!(refused.error_code(), Some("workflow_required"));

    approved(&env.user, &name).await;
    let refused = env
        .manager
        .post(&format!("{ORDERS}/{name}/cancel"), json!({"version": 3}))
        .await;
    assert_eq!(refused.status, 409, "{:?}", refused.body);
    assert_eq!(refused.error_code(), Some("workflow_required"));
    let read = env.user.get(&format!("{ORDERS}/{name}")).await;
    assert_eq!(read.data()["docstatus"], 1);
    env.db.cleanup().await;
}

#[tokio::test]
async fn a_sales_user_approves_up_to_the_limit_and_a_manager_above_it() {
    let env = env().await;
    customer(&env.manager).await;
    let name = draft(&env, None, big_items()).await;
    let sent = transition(&env.user, &name, "Submit for Approval", 1).await;
    assert_eq!(sent.status, 200, "{:?}", sent.body);
    assert_eq!(sent.data()["total"], "10000.01");

    // The hook vetoes: the message names the way out, and nothing changed (state, version, log,
    // jobs).
    let vetoed = transition(&env.user, &name, "Approve", 2).await;
    assert_eq!(vetoed.status, 422, "{:?}", vetoed.body);
    assert_eq!(vetoed.error_code(), Some("hook_rejected"));
    assert!(
        vetoed
            .error_message()
            .expect("message")
            .contains("need a Sales Manager: use 'Approve as Manager'"),
        "{:?}",
        vetoed.body
    );
    let read = env.user.get(&format!("{ORDERS}/{name}")).await;
    assert_eq!(read.data()["workflow_state"], "Pending Approval");
    assert_eq!(read.data()["version"], 2);
    assert_eq!(
        log_of(&env.db, &name).await,
        ["Submit for Approval: Draft -> Pending Approval by ann"]
    );
    assert_eq!(
        count(&env.db, "SELECT count(*)::text FROM fb_job").await,
        "0"
    );

    // The manager's action is not open to a Sales User: it needs the role.
    let denied = transition(&env.user, &name, "Approve as Manager", 2).await;
    assert_eq!(denied.status, 403, "{:?}", denied.body);
    assert_eq!(denied.error_code(), Some("permission_denied"));

    // The Sales Manager approves the large order with the action that has no limit.
    let done = transition(&env.manager, &name, "Approve as Manager", 2).await;
    assert_eq!(done.status, 200, "{:?}", done.body);
    assert_eq!(done.data()["workflow_state"], "Approved");
    assert_eq!(done.data()["docstatus"], 1);
    assert_eq!(
        log_of(&env.db, &name).await.last().map(String::as_str),
        Some("Approve as Manager: Pending Approval -> Approved by max")
    );

    // An order of exactly the limit is a Sales User's to approve.
    let exact = draft(
        &env,
        None,
        json!([{"description": "Machine", "qty": 1, "rate": "10000.00"}]),
    )
    .await;
    approved(&env.user, &exact).await;
    env.db.cleanup().await;
}

#[tokio::test]
async fn a_rejected_order_is_a_draft_again_and_editable() {
    let env = env().await;
    customer(&env.manager).await;
    let name = draft(&env, None, two_items()).await;
    transition(&env.user, &name, "Submit for Approval", 1).await;

    // While it waits for approval only a Sales Manager may edit it (`edit_roles`): a Sales User is
    // refused, for the remarks and for the items alike.
    let edit = json!({"version": 2, "data": {"remarks": "ring me"}});
    let denied = env
        .user
        .patch(&format!("{ORDERS}/{name}"), edit.clone())
        .await;
    assert_eq!(denied.status, 403, "{:?}", denied.body);
    assert_eq!(denied.error_code(), Some("permission_denied"));
    let new_items = json!({"version": 2, "data": {
        "items": [{"description": "Cheaper", "qty": 1, "rate": "1.00"}]
    }});
    let denied = env.user.patch(&format!("{ORDERS}/{name}"), new_items).await;
    assert_eq!(denied.status, 403, "{:?}", denied.body);
    assert_eq!(denied.error_code(), Some("permission_denied"));

    // A Sales Manager may edit it (version 2 to 3).
    let note = json!({"version": 2, "data": {"remarks": "checked by the manager"}});
    let edited = env.manager.patch(&format!("{ORDERS}/{name}"), note).await;
    assert_eq!(edited.status, 200, "{:?}", edited.body);
    assert_eq!(edited.data()["remarks"], "checked by the manager");
    assert_eq!(edited.data()["workflow_state"], "Pending Approval");

    // Only a Sales Manager rejects.
    let denied = transition(&env.user, &name, "Reject", 3).await;
    assert_eq!(denied.status, 403, "{:?}", denied.body);
    assert_eq!(denied.error_code(), Some("permission_denied"));
    let rejected = transition(&env.manager, &name, "Reject", 3).await;
    assert_eq!(rejected.status, 200, "{:?}", rejected.body);
    assert_eq!(rejected.data()["workflow_state"], "Draft");
    assert_eq!(rejected.data()["docstatus"], 0);

    // Back in Draft the Sales User edits it again.
    let edit = json!({"version": 4, "data": {"remarks": "ring me"}});
    let updated = env.user.patch(&format!("{ORDERS}/{name}"), edit).await;
    assert_eq!(updated.status, 200, "{:?}", updated.body);
    assert_eq!(updated.data()["remarks"], "ring me");
    env.db.cleanup().await;
}

#[tokio::test]
async fn an_order_without_a_total_cannot_be_approved_with_the_limited_action() {
    let env = env().await;
    customer(&env.manager).await;
    let name = draft(&env, None, two_items()).await;
    let sent = transition(&env.user, &name, "Submit for Approval", 1).await;
    assert_eq!(sent.status, 200, "{:?}", sent.body);
    // `validate` always computes the total; an order without one was changed outside the
    // framework (here: in SQL).
    env.db
        .exec(&format!(
            "UPDATE doc_sales_order SET total = NULL WHERE name = '{name}'"
        ))
        .await;
    let vetoed = transition(&env.user, &name, "Approve", 2).await;
    assert_eq!(vetoed.status, 422, "{:?}", vetoed.body);
    assert_eq!(vetoed.error_code(), Some("hook_rejected"));
    assert!(
        vetoed
            .error_message()
            .expect("message")
            .contains("the order has no total"),
        "{:?}",
        vetoed.body
    );
    let read = env.user.get(&format!("{ORDERS}/{name}")).await;
    assert_eq!(read.data()["workflow_state"], "Pending Approval");
    env.db.cleanup().await;
}

#[tokio::test]
async fn a_wrong_state_and_an_unknown_action_are_refused() {
    let env = env().await;
    customer(&env.manager).await;
    let name = draft(&env, None, two_items()).await;
    // "Approve" is not a transition from Draft; "Fly" is no action at all.
    for action in ["Approve", "Fly"] {
        let refused = transition(&env.user, &name, action, 1).await;
        assert_eq!(refused.status, 409, "{:?}", refused.body);
        assert_eq!(refused.error_code(), Some("unknown_transition"));
    }
    // A user without a sales role cannot even see the order.
    let hidden = transition(&env.outsider, &name, "Submit for Approval", 1).await;
    assert_eq!(hidden.status, 403, "{:?}", hidden.body);
    env.db.cleanup().await;
}

// --- The report "Sales by customer" -------------------------------------------------------------

const REPORT: &str = "/api/v1/report/sales.sales_by_customer";

/// An order of `customer` on `date` with one item of `qty` x `rate`, approved when `approve`.
async fn order_of(env: &Env, customer: &str, date: &str, rate: &str, approve: bool) -> String {
    let created = env
        .user
        .post(
            ORDERS,
            json!({"customer": customer, "order_date": date,
                   "items": [{"description": "Item", "qty": 1, "rate": rate}]}),
        )
        .await;
    assert_eq!(created.status, 201, "{:?}", created.body);
    let name = created.data()["name"].as_str().expect("name").to_owned();
    if approve {
        approved(&env.user, &name).await;
    }
    name
}

#[tokio::test]
async fn the_report_sql_fits_its_declaration() {
    let env = env().await;
    assert_eq!(
        check_reports(&binary(), &env.db).await,
        Vec::<String>::new()
    );
    env.db.cleanup().await;
}

#[tokio::test]
async fn sales_by_customer_counts_the_approved_orders_of_the_period() {
    let env = env().await;
    customer(&env.manager).await;
    let reply = env
        .manager
        .post(
            "/api/v1/resource/customer",
            json!({"email": "bob@example.com", "full_name": "Bob"}),
        )
        .await;
    assert_eq!(reply.status, 201, "{:?}", reply.body);

    // Counted: two orders of ada (12.00 and 5.00) and one of bob (40.00) in October.
    order_of(&env, "ada@example.com", "2026-10-01", "12.00", true).await;
    order_of(&env, "ada@example.com", "2026-10-05", "5.00", true).await;
    order_of(&env, "bob@example.com", "2026-10-03", "40.00", true).await;
    // Not counted: a draft, a pending order, a cancelled order, an order outside the period.
    order_of(&env, "ada@example.com", "2026-10-06", "100.00", false).await;
    let pending = order_of(&env, "ada@example.com", "2026-10-07", "100.00", false).await;
    let sent = transition(&env.user, &pending, "Submit for Approval", 1).await;
    assert_eq!(sent.status, 200, "{:?}", sent.body);
    let cancelled = order_of(&env, "ada@example.com", "2026-10-08", "100.00", true).await;
    let reply = transition(&env.manager, &cancelled, "Cancel", 3).await;
    assert_eq!(reply.status, 200, "{:?}", reply.body);
    order_of(&env, "ada@example.com", "2026-12-01", "100.00", true).await;

    // Biggest customer first.
    let run = env
        .manager
        .post(
            REPORT,
            json!({"params": {"from": "2026-10-01", "to": "2026-10-31"}}),
        )
        .await;
    assert_eq!(run.status, 200, "{:?}", run.body);
    assert_eq!(
        run.data()["columns"]
            .as_array()
            .expect("columns")
            .iter()
            .map(|c| c["name"].as_str().expect("name"))
            .collect::<Vec<_>>(),
        ["customer", "orders", "total", "last_order"]
    );
    assert_eq!(
        run.data()["rows"],
        json!([
            ["bob@example.com", 1, "40.00", "2026-10-03"],
            ["ada@example.com", 2, "17.00", "2026-10-05"]
        ])
    );
    assert_eq!(run.data()["truncated"], false);

    // The optional customer narrows it; the dates are required.
    let one = env
        .manager
        .post(
            REPORT,
            json!({"params": {"from": "2026-10-01", "to": "2026-10-31",
                              "customer": "bob@example.com"}}),
        )
        .await;
    assert_eq!(
        one.data()["rows"],
        json!([["bob@example.com", 1, "40.00", "2026-10-03"]])
    );
    let missing = env
        .manager
        .post(REPORT, json!({"params": {"from": "2026-10-01"}}))
        .await;
    assert_eq!(missing.status, 422, "{:?}", missing.body);
    env.db.cleanup().await;
}

#[tokio::test]
async fn a_sales_user_cannot_run_or_see_the_report() {
    let env = env().await;
    let params = json!({"params": {"from": "2026-10-01", "to": "2026-10-31"}});
    // Not allowed looks like not existing (404), for the Sales User and for a stranger.
    for client in [&env.user, &env.outsider] {
        let refused = client.post(REPORT, params.clone()).await;
        assert_eq!(refused.status, 404, "{:?}", refused.body);
        let listed = client.get("/api/v1/report").await;
        assert_eq!(listed.status, 200, "{:?}", listed.body);
        assert_eq!(listed.data(), &json!([]));
    }
    let listed = env.manager.get("/api/v1/report").await;
    assert_eq!(listed.data()[0]["name"], "sales.sales_by_customer");
    assert_eq!(listed.data()[0]["label"], "Sales by customer");
    env.db.cleanup().await;
}

// --- Jobs and the schedule ----------------------------------------------------------------------

fn binary() -> Forgebase {
    Forgebase::new().app(app())
}

/// Creates an order of `items` (and `order_date` when given) as the Sales User and returns its
/// name. The first item is 3 x 10.50 and the second 2 x 0.25, so `two_items` totals 32.00.
async fn draft(env: &Env, order_date: Option<&str>, items: forgebase::testing::Json) -> String {
    let mut body = json!({"customer": "ada@example.com", "items": items});
    if let Some(date) = order_date {
        body["order_date"] = json!(date);
    }
    let created = env.user.post(ORDERS, body).await;
    assert_eq!(created.status, 201, "{:?}", created.body);
    created.data()["name"].as_str().expect("name").to_owned()
}

/// Sends the draft for approval and approves it (version 1 to 3): the order is submitted.
async fn submit(env: &Env, name: &str) {
    approved(&env.user, name).await;
}

/// Copies the job rows named `name` into new queued rows: the same job delivered twice.
async fn duplicate_jobs(db: &TestDb, name: &str) {
    db.exec(&format!(
        "INSERT INTO fb_job (id, name, payload, status, enqueued_by)
         SELECT gen_random_uuid(), name, payload, 'queued', 'test' FROM fb_job
         WHERE name = '{name}'"
    ))
    .await;
}

async fn count(db: &TestDb, sql: &str) -> String {
    db.rows(sql).await.remove(0).remove(0)
}

#[tokio::test]
async fn a_submit_and_a_cancel_are_logged_by_a_job() {
    let env = env().await;
    customer(&env.manager).await;
    let name = draft(&env, None, two_items()).await;
    submit(&env, &name).await;

    // The outbox: the job exists, queued, as soon as the submit committed.
    let queued = env
        .db
        .rows("SELECT name, status, enqueued_by FROM fb_job")
        .await;
    assert_eq!(queued, [["sales.log_order_event", "queued", "ann"]]);

    let run = run_jobs(&binary(), &env.db).await;
    assert_eq!((run.done(), run.retried(), run.dead()), (1, 0, 0));
    let logs = env
        .db
        .rows("SELECT name, event, total::text, owner, modified_by FROM doc_sales_order_log")
        .await;
    assert_eq!(
        logs,
        [[
            format!("{name}-submitted").as_str(),
            "submitted",
            "32.00",
            "job:sales.log_order_event",
            "job:sales.log_order_event"
        ]]
    );
    // The log links to the order, and a Sales User can read it but not change it.
    let read = env
        .user
        .get(&format!(
            "/api/v1/resource/sales_order_log/{name}-submitted"
        ))
        .await;
    assert_eq!(read.status, 200, "{:?}", read.body);
    assert_eq!(read.data()["sales_order"], name.as_str());
    let write = env
        .user
        .post(
            "/api/v1/resource/sales_order_log",
            json!({"key": "x", "sales_order": name}),
        )
        .await;
    assert_eq!(write.status, 403, "{:?}", write.body);

    let cancelled = transition(&env.manager, &name, "Cancel", 3).await;
    assert_eq!(cancelled.status, 200, "{:?}", cancelled.body);
    let run = run_jobs(&binary(), &env.db).await;
    assert_eq!(run.done(), 1);
    let events = env
        .db
        .rows("SELECT event FROM doc_sales_order_log ORDER BY event COLLATE \"C\"")
        .await;
    assert_eq!(events, [["cancelled"], ["submitted"]]);
    env.db.cleanup().await;
}

#[tokio::test]
async fn a_job_delivered_twice_logs_once() {
    let env = env().await;
    customer(&env.manager).await;
    let name = draft(&env, None, two_items()).await;
    submit(&env, &name).await;
    // At-least-once delivery: the same job twice, as after a worker crash.
    duplicate_jobs(&env.db, "sales.log_order_event").await;

    let run = run_jobs(&binary(), &env.db).await;
    assert_eq!((run.done(), run.retried(), run.dead()), (2, 0, 0));
    assert_eq!(
        count(&env.db, "SELECT count(*)::text FROM doc_sales_order_log").await,
        "1"
    );
    let status = env
        .db
        .rows("SELECT status, count(*)::text FROM fb_job GROUP BY status")
        .await;
    assert_eq!(status, [["done", "2"]]);
    env.db.cleanup().await;
}

#[tokio::test]
async fn a_refused_submit_leaves_no_job() {
    // The refusal happens before any job is collected. "Collected, then refused" is covered by
    // the framework test `forgebase-db/tests/jobs.rs::a_later_rejecting_hook_leaves_no_job`.
    let env = env().await;
    customer(&env.manager).await;
    // The rule "an order needs items" refuses the approval, which is the submit.
    let name = draft(&env, None, json!([])).await;
    let sent = transition(&env.user, &name, "Submit for Approval", 1).await;
    assert_eq!(sent.status, 200, "{:?}", sent.body);
    let refused = transition(&env.user, &name, "Approve", 2).await;
    assert_eq!(refused.status, 422, "{:?}", refused.body);
    assert_eq!(
        count(
            &env.db,
            "SELECT count(*)::text FROM fb_job WHERE name = 'sales.log_order_event'"
        )
        .await,
        "0"
    );
    env.db.cleanup().await;
}

#[tokio::test]
async fn a_job_for_a_missing_order_is_dead_at_once() {
    let env = env().await;
    env.db
        .exec(
            "INSERT INTO fb_job (id, name, payload, status, enqueued_by)
             VALUES (gen_random_uuid(), 'sales.log_order_event',
                     '{\"order\": \"SO-0\", \"event\": \"submitted\"}', 'queued', 'test'),
                    (gen_random_uuid(), 'sales.log_order_event', '{}', 'queued', 'test')",
        )
        .await;
    let run = run_jobs(&binary(), &env.db).await;
    assert_eq!((run.done(), run.retried(), run.dead()), (0, 0, 2));
    let errors = env
        .db
        .rows("SELECT attempts::text, last_error FROM fb_job ORDER BY last_error COLLATE \"C\"")
        .await;
    assert_eq!(errors.len(), 2);
    assert!(errors.iter().all(|row| row[0] == "1"), "{errors:?}");
    assert!(
        errors[0][1].starts_with("the payload must be"),
        "{errors:?}"
    );
    assert_eq!(errors[1][1], "the sales order SO-0 does not exist");
    env.db.cleanup().await;
}

#[tokio::test]
async fn the_daily_summary_counts_the_submitted_orders_of_the_day_before_the_tick() {
    let env = env().await;
    customer(&env.manager).await;

    // `migrate` stored the schedule: due at the next 02:00 UTC. The summary is of the day
    // before that tick. Both are read from the database, so the test does not depend on when
    // it runs.
    let due = env
        .db
        .rows(
            "SELECT ((next_run_at AT TIME ZONE 'UTC')::date - 1)::text,
                    extract(epoch FROM next_run_at)::bigint::text
             FROM fb_schedule WHERE name = 'sales.daily_summary'",
        )
        .await;
    let [day, tick] = &due[0][..] else {
        panic!("{due:?}")
    };
    let tick = OffsetDateTime::from_unix_timestamp(tick.parse().expect("epoch")).expect("time");

    // Two submitted orders on the day (32.00 and 5.00) are counted.
    for items in [
        two_items(),
        json!([{"description": "Pen", "qty": 1, "rate": "5.00"}]),
    ] {
        let name = draft(&env, Some(day), items).await;
        submit(&env, &name).await;
    }
    // Not counted: a draft of the day, a cancelled order of the day, a submitted order of
    // another day.
    draft(&env, Some(day), two_items()).await;
    let cancelled = draft(&env, Some(day), two_items()).await;
    submit(&env, &cancelled).await;
    let reply = transition(&env.manager, &cancelled, "Cancel", 3).await;
    assert_eq!(reply.status, 200, "{:?}", reply.body);
    let other = draft(&env, Some("2000-01-01"), two_items()).await;
    submit(&env, &other).await;

    // The tick: the schedule enqueues the job once, and a second call for the same time does
    // not enqueue it again.
    assert!(run_schedules_at(&binary(), &env.db, tick).await >= 1);
    assert_eq!(run_schedules_at(&binary(), &env.db, tick).await, 0);
    let queued = env
        .db
        .rows(
            "SELECT count(*)::text, min(payload->>'scheduled_for') FROM fb_job
             WHERE name = 'sales.daily_summary'",
        )
        .await;
    assert_eq!(queued[0][0], "1");
    assert!(queued[0][1].starts_with(&format!("{}T02:00:00", tick.date())));

    let run = run_jobs(&binary(), &env.db).await;
    assert_eq!(run.dead(), 0);
    let summary = env
        .db
        .rows("SELECT name, day, orders::text, total::text, owner FROM doc_sales_daily_summary")
        .await;
    assert_eq!(
        summary,
        [[
            day.as_str(),
            day.as_str(),
            "2",
            "37.00",
            "job:sales.daily_summary"
        ]]
    );

    // The same tick delivered again (an operator enqueued it): the handler finds the summary
    // and does nothing, and the job still ends `done`.
    duplicate_jobs(&env.db, "sales.daily_summary").await;
    let run = run_jobs(&binary(), &env.db).await;
    assert_eq!((run.done(), run.dead()), (1, 0));
    assert_eq!(
        count(
            &env.db,
            "SELECT count(*)::text FROM doc_sales_daily_summary"
        )
        .await,
        "1"
    );
    env.db.cleanup().await;
}

// --- The method sales.order_summary -------------------------------------------------------------

const SUMMARY: &str = "/api/v1/method/sales.order_summary";

/// Four orders in four states, made by the Sales User (so the Sales Manager's rules are not the
/// only ones at work): Ada's draft of 10.00 on 2026-10-05, Ada's pending order of 20.50 on
/// 2026-10-10, Bob's approved order of 5.00 on 2026-11-02 and Ada's cancelled order of 1.10 on
/// 2026-10-20. Together 36.60.
async fn four_orders(env: &Env) {
    customer(&env.manager).await;
    let reply = env
        .manager
        .post(
            "/api/v1/resource/customer",
            json!({"email": "bob@example.com", "full_name": "Bob"}),
        )
        .await;
    assert_eq!(reply.status, 201, "{:?}", reply.body);
    order_of(env, "ada@example.com", "2026-10-05", "10.00", false).await;
    let pending = order_of(env, "ada@example.com", "2026-10-10", "20.50", false).await;
    let sent = transition(&env.user, &pending, "Submit for Approval", 1).await;
    assert_eq!(sent.status, 200, "{:?}", sent.body);
    order_of(env, "bob@example.com", "2026-11-02", "5.00", true).await;
    let cancelled = order_of(env, "ada@example.com", "2026-10-20", "1.10", true).await;
    let done = transition(&env.manager, &cancelled, "Cancel", 3).await;
    assert_eq!(done.status, 200, "{:?}", done.body);
}

#[test]
fn the_summary_states_are_the_workflow_states() {
    // STATES is written out in the app; the DocType file is the truth.
    let doctype = include_str!("../doctypes/sales_order.toml");
    for state in ["Draft", "Pending Approval", "Approved", "Cancelled"] {
        assert!(
            doctype.contains(&format!("[[workflow.states]]\nname = \"{state}\"")),
            "{state} is not a workflow state of sales_order"
        );
    }
    assert_eq!(doctype.matches("[[workflow.states]]").count(), 4);
}

#[tokio::test]
async fn the_summary_counts_orders_totals_and_states() {
    let env = env().await;
    four_orders(&env).await;

    let all = env.user.post(SUMMARY, json!({})).await;
    assert_eq!(all.status, 200, "{:?}", all.body);
    assert_eq!(
        all.data(),
        &json!({"orders": 4, "total": "36.60",
                "by_state": {"Draft": 1, "Pending Approval": 1, "Approved": 1, "Cancelled": 1}})
    );
    // The total is a string with its scale, not a float.
    assert!(all.data()["total"].is_string());

    // One customer; the bounds are both included; null is the same as absent.
    let ada = env
        .user
        .post(
            SUMMARY,
            json!({"customer": "ada@example.com", "from": null}),
        )
        .await;
    assert_eq!(ada.data()["orders"], 3, "{:?}", ada.body);
    assert_eq!(ada.data()["total"], "31.60");
    let october = env
        .user
        .post(SUMMARY, json!({"from": "2026-10-10", "to": "2026-10-20"}))
        .await;
    assert_eq!(
        october.data(),
        &json!({"orders": 2, "total": "21.60",
                "by_state": {"Draft": 0, "Pending Approval": 1, "Approved": 0, "Cancelled": 1}})
    );
    let none = env
        .user
        .post(SUMMARY, json!({"customer": "nobody@example.com"}))
        .await;
    assert_eq!(
        none.data(),
        &json!({"orders": 0, "total": "0.00",
                "by_state": {"Draft": 0, "Pending Approval": 0, "Approved": 0, "Cancelled": 0}})
    );

    // A Sales Manager is allowed too, and sees the same orders; so does a request without a
    // body at all.
    let manager = env.manager.post(SUMMARY, json!({})).await;
    assert_eq!(manager.data(), all.data());
    let no_body = env.user.post_raw(SUMMARY, None, b"").await;
    assert_eq!(no_body.data(), all.data(), "{:?}", no_body.body);
    env.db.cleanup().await;
}

#[tokio::test]
async fn the_summary_counts_what_the_roles_can_read() {
    // `sales_order` has no `if_owner` rule: the read right of Sales User and Sales Manager is for
    // every order, so an order of the manager is counted for the Sales User too. (The handler
    // reads through the caller's documents handle, so an `if_owner` rule would narrow the count
    // with no change to it; the framework tests `if_owner` inside a method handler.)
    let env = env().await;
    customer(&env.manager).await;
    let created = env
        .manager
        .post(
            ORDERS,
            json!({"customer": "ada@example.com", "items": two_items()}),
        )
        .await;
    assert_eq!(created.status, 201, "{:?}", created.body);
    let seen = env.user.post(SUMMARY, json!({})).await;
    assert_eq!(seen.data()["orders"], 1, "{:?}", seen.body);
    assert_eq!(seen.data()["total"], "32.00");
    assert_eq!(seen.data()["by_state"]["Draft"], 1);
    env.db.cleanup().await;
}

#[tokio::test]
async fn the_summary_is_closed_to_everyone_else() {
    let env = env().await;
    four_orders(&env).await;

    // No session: 401.
    let anonymous = env.anon.post(SUMMARY, json!({})).await;
    assert_eq!(anonymous.status, 401, "{:?}", anonymous.body);
    // A role that is not in the method's list: the method does not exist for this user, 404.
    let outsider = env.outsider.post(SUMMARY, json!({})).await;
    assert_eq!(outsider.status, 404, "{:?}", outsider.body);
    // An unknown method is 404 as well, so the two cannot be told apart.
    let unknown = env
        .user
        .post("/api/v1/method/sales.nothing", json!({}))
        .await;
    assert_eq!(unknown.status, 404, "{:?}", unknown.body);
    assert_eq!(unknown.body, outsider.body);
    // A cookie call needs the CSRF header.
    let no_csrf = env.user.without_csrf().post(SUMMARY, json!({})).await;
    assert_eq!(no_csrf.status, 403, "{:?}", no_csrf.body);
    // An API token needs no CSRF header.
    let token = env.db.create_token("ann").await;
    let by_token = env.anon.with_token(&token).post(SUMMARY, json!({})).await;
    assert_eq!(by_token.status, 200, "{:?}", by_token.body);
    assert_eq!(by_token.data()["orders"], 4);
    // The token of the Accountant is no better than the session.
    let eve = env.db.create_token("eve").await;
    let denied = env.anon.with_token(&eve).post(SUMMARY, json!({})).await;
    assert_eq!(denied.status, 404, "{:?}", denied.body);
    env.db.cleanup().await;
}

#[tokio::test]
async fn the_summary_refuses_a_bad_input_with_a_clear_message() {
    let env = env().await;
    let cases = [
        (json!({"from": "2026-13-01"}), "from must be a date"),
        (json!({"from": "2026-10-1"}), "from must be a date"),
        (json!({"to": "yesterday"}), "to must be a date"),
        (json!({"to": 20_261_031}), "to must be a date"),
        (json!({"customer": 5}), "customer must be the name"),
        (json!({"customer": "  "}), "customer must be the name"),
        (json!({"limit": 10}), "unknown key 'limit'"),
        (
            json!({"from": "2026-10-31", "to": "2026-10-01"}),
            "from must not be after to",
        ),
    ];
    for (body, message) in cases {
        let reply = env.user.post(SUMMARY, body.clone()).await;
        assert_eq!(reply.status, 400, "{body}: {:?}", reply.body);
        assert_eq!(reply.error_code(), Some("method_error"), "{body}");
        assert!(
            reply.error_message().is_some_and(|m| m.contains(message)),
            "{body}: {:?}",
            reply.body
        );
    }
    // A body that is not an object.
    let array = env.user.post(SUMMARY, json!([1])).await;
    assert_eq!(array.status, 400, "{:?}", array.body);
    env.db.cleanup().await;
}

#[tokio::test]
async fn the_summary_pages_through_more_than_one_page_of_orders() {
    let env = env().await;
    customer(&env.manager).await;
    // 1 001 drafts of 2.00 written with SQL (through the API it would take minutes; a Link column
    // holds the id of the target): two full
    // pages of 500 and a page of one, which is where a paging mistake shows. One pending order
    // of 3.00 through the API.
    env.db
        .exec(
            "INSERT INTO doc_sales_order
               (id, name, owner, created_at, modified_at, modified_by, customer, total, workflow_state)
             SELECT gen_random_uuid(), 'SO-BULK-' || lpad(n::text, 5, '0'), 'ann', now(), now(), 'ann',
                    (SELECT id FROM doc_customer WHERE name = 'ada@example.com'), 2.00, 'Draft'
             FROM generate_series(1, 1001) AS n",
        )
        .await;
    let pending = order_of(&env, "ada@example.com", "2026-10-05", "3.00", false).await;
    let sent = transition(&env.user, &pending, "Submit for Approval", 1).await;
    assert_eq!(sent.status, 200, "{:?}", sent.body);

    let reply = env.user.post(SUMMARY, json!({})).await;
    assert_eq!(reply.status, 200, "{:?}", reply.body);
    assert_eq!(
        reply.data(),
        &json!({"orders": 1002, "total": "2005.00",
                "by_state": {"Draft": 1001, "Pending Approval": 1, "Approved": 0, "Cancelled": 0}})
    );
    env.db.cleanup().await;
}
