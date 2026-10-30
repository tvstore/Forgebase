//! Tests of the `sales` app. The ones that need PostgreSQL (see README.md) fail, they do not
//! skip, when `FORGEBASE_TEST_DATABASE_URL` is not set.

use std::str::FromStr;

use forgebase::testing::{
    HttpClient, TestDb, assert_doctypes_listed, json, run_jobs, run_schedules_at,
};
use forgebase::{
    Decimal, DocTypeHooks, Document, EnqueuedJobs, Forgebase, HookContext, OffsetDateTime,
    Operation, Row, Value,
};

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

async fn submit(env: &Env, name: &str) {
    let reply = env
        .user
        .post(&format!("{ORDERS}/{name}/submit"), json!({"version": 1}))
        .await;
    assert_eq!(reply.status, 200, "{:?}", reply.body);
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

    let cancelled = env
        .manager
        .post(&format!("{ORDERS}/{name}/cancel"), json!({"version": 2}))
        .await;
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
    // The rule "an order needs items" refuses this submit.
    let name = draft(&env, None, json!([])).await;
    let refused = env
        .user
        .post(&format!("{ORDERS}/{name}/submit"), json!({"version": 1}))
        .await;
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
    let reply = env
        .manager
        .post(
            &format!("{ORDERS}/{cancelled}/cancel"),
            json!({"version": 2}),
        )
        .await;
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
