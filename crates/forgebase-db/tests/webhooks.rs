//! Outgoing webhooks, the storage and the outbox (M9.2, ADR 0020) against a real PostgreSQL: the
//! `fb_webhook` table, and the delivery jobs that every document operation enqueues in its own
//! transaction. Nothing is sent here: the delivery is tested in the `forgebase` crate.

mod common;

use std::collections::BTreeMap;

use common::{TestDb, registry};
use forgebase_core::document::{Changes, Document};
use forgebase_core::meta::Registry;
use forgebase_core::permission::Caller;
use forgebase_core::value::Value;
use forgebase_db::webhooks::{self, DELIVER_JOB, NewWebhook};
use forgebase_db::{DbError, amend, cancel, delete, insert, submit, transition, update};

const NOTE: &str = r#"
name = "note"
label = "Note"
[[fields]]
name = "title"
type = "data"
required = true
[[fields]]
name = "body"
type = "text"
"#;

const LINE: &str = r#"
name = "invoice_line"
label = "Invoice Line"
is_child = true
[[fields]]
name = "item"
type = "data"
"#;

const INVOICE: &str = r#"
name = "invoice"
label = "Invoice"
submittable = true
[[fields]]
name = "title"
type = "data"
required = true
[[fields]]
name = "lines"
type = "table"
target = "invoice_line"
"#;

const ORDER: &str = r#"
name = "order"
label = "Order"
submittable = true
[[fields]]
name = "title"
type = "data"
required = true

[workflow]
[[workflow.states]]
name = "Draft"
docstatus = 0
[[workflow.states]]
name = "Review"
docstatus = 0
[[workflow.states]]
name = "Done"
docstatus = 1
[[workflow.states]]
name = "Void"
docstatus = 2
[[workflow.transitions]]
action = "Send"
from = "Draft"
to = "Review"
roles = ["Manager"]
[[workflow.transitions]]
action = "Finish"
from = "Review"
to = "Done"
roles = ["Manager"]
[[workflow.transitions]]
action = "Revoke"
from = "Done"
to = "Void"
roles = ["Manager"]
"#;

fn world() -> Registry {
    let mut registry = registry(&[NOTE, LINE, INVOICE, ORDER]);
    assert!(registry.add_job(DELIVER_JOB));
    registry
}

async fn setup() -> (TestDb, Registry) {
    let db = TestDb::migrated().await;
    let registry = world();
    db.sync(&registry).await.expect("sync");
    (db, registry)
}

fn text(s: &str) -> Value {
    Value::Text(s.to_owned())
}

fn doc(title: &str) -> Document {
    Document {
        name: None,
        values: BTreeMap::from([("title".to_owned(), text(title))]),
        tables: BTreeMap::new(),
    }
}

fn title(value: &str) -> Changes {
    Changes {
        values: BTreeMap::from([("title".to_owned(), text(value))]),
        tables: BTreeMap::new(),
    }
}

fn events(list: &[&str]) -> Vec<String> {
    list.iter().map(|e| (*e).to_owned()).collect()
}

async fn hook(db: &TestDb, registry: &Registry, name: &str, doctype: &str, list: &[&str]) {
    let client = db.pool.get().await.unwrap();
    let list = events(list);
    webhooks::add(
        &**client,
        registry,
        &NewWebhook {
            name,
            doctype,
            events: &list,
            url: "https://example.com/hook",
            allow_private: false,
            insecure_http: false,
        },
        None,
    )
    .await
    .expect("add");
}

/// `webhook/event/doctype/name/enqueued_by/status` of every delivery job, oldest first.
async fn deliveries(db: &TestDb) -> Vec<String> {
    db.rows(
        "SELECT payload->>'webhook' || '/' || (payload->>'event') || '/' || (payload->>'doctype') \
         || '/' || (payload->>'name') || '/' || enqueued_by || '/' || status FROM fb_job \
         WHERE name = 'forgebase.deliver_webhook' ORDER BY id",
    )
    .await
    .into_iter()
    .map(|mut r| r.remove(0))
    .collect()
}

#[tokio::test]
async fn adding_checks_the_name_the_events_and_the_doctype() {
    let (db, registry) = setup().await;
    let client = db.pool.get().await.unwrap();
    let list = events(&["insert"]);
    let new = |name: &'static str, doctype: &'static str, events: &'static [String]| NewWebhook {
        name,
        doctype,
        events,
        url: "https://example.com/x",
        allow_private: false,
        insecure_http: false,
    };
    let leaked: &'static [String] = Box::leak(list.into_boxed_slice());

    // A good one: the secret is 32 random bytes as 64 hex characters.
    let secret = webhooks::add(&**client, &registry, &new("good", "note", leaked), None)
        .await
        .expect("added");
    assert_eq!(secret.len(), 64);
    assert!(secret.bytes().all(|b| b.is_ascii_hexdigit()));
    // A given secret is used as it is.
    let given = webhooks::add(
        &**client,
        &registry,
        &new("given", "note", leaked),
        Some("my-own-secret"),
    )
    .await
    .expect("added");
    assert_eq!(given, "my-own-secret");

    let refused = |error: DbError| match error {
        DbError::Webhook(message) => message,
        other => other.to_string(),
    };
    // The name is taken, or not a valid name.
    let error = webhooks::add(&**client, &registry, &new("good", "note", leaked), None)
        .await
        .expect_err("taken");
    assert!(refused(error).contains("exists already"));
    for bad in ["", "Bad", "has space", "1abc", "a-b"] {
        webhooks::add(&**client, &registry, &new(bad, "note", leaked), None)
            .await
            .expect_err(bad);
    }
    // The DocType must exist and must not be a child DocType.
    assert!(matches!(
        webhooks::add(&**client, &registry, &new("a", "nobody", leaked), None).await,
        Err(DbError::UnknownDocType { .. })
    ));
    assert!(matches!(
        webhooks::add(
            &**client,
            &registry,
            &new("b", "invoice_line", leaked),
            None
        )
        .await,
        Err(DbError::ChildDocType { .. })
    ));
    // The events: at least one, all known.
    let none: &'static [String] = &[];
    let unknown: &'static [String] = Box::leak(events(&["insert", "save"]).into_boxed_slice());
    for bad in [none, unknown] {
        webhooks::add(&**client, &registry, &new("c", "note", bad), None)
            .await
            .expect_err("events");
    }
    // The database refuses an empty or unknown events list as well, whoever writes the row.
    let direct = client
        .execute(
            "INSERT INTO fb_webhook (name, doctype, events, url, secret) \
             VALUES ('raw', 'note', '{}', 'https://x', 's')",
            &[],
        )
        .await;
    assert!(direct.is_err(), "the CHECK constraint holds");
    let direct = client
        .execute(
            "INSERT INTO fb_webhook (name, doctype, events, url, secret) \
             VALUES ('raw', 'note', '{save}', 'https://x', 's')",
            &[],
        )
        .await;
    assert!(direct.is_err(), "the CHECK constraint holds");

    // Nothing but the two good ones was stored.
    let all = webhooks::list(&**client).await.unwrap();
    assert_eq!(
        all.iter().map(|w| w.name.as_str()).collect::<Vec<_>>(),
        ["given", "good"]
    );
    assert!(
        all.iter()
            .all(|w| w.enabled && !w.allow_private && !w.insecure_http)
    );
    let loaded = webhooks::get(&**client, "given").await.unwrap().unwrap();
    assert_eq!(loaded.secret, "my-own-secret");
    assert!(webhooks::get(&**client, "nobody").await.unwrap().is_none());
}

#[tokio::test]
async fn enable_disable_remove_and_rotate_report_a_missing_webhook() {
    let (db, registry) = setup().await;
    hook(&db, &registry, "one", "note", &["insert"]).await;
    let client = db.pool.get().await.unwrap();
    webhooks::set_enabled(&**client, "one", false)
        .await
        .unwrap();
    assert!(
        !webhooks::get(&**client, "one")
            .await
            .unwrap()
            .unwrap()
            .enabled
    );
    webhooks::set_enabled(&**client, "one", true).await.unwrap();
    let old = webhooks::get(&**client, "one")
        .await
        .unwrap()
        .unwrap()
        .secret;
    let new = webhooks::rotate_secret(&**client, "one", None)
        .await
        .unwrap();
    assert_ne!(old, new);
    assert_eq!(
        webhooks::get(&**client, "one")
            .await
            .unwrap()
            .unwrap()
            .secret,
        new
    );
    webhooks::remove(&**client, "one").await.unwrap();
    for result in [
        webhooks::set_enabled(&**client, "one", true).await,
        webhooks::remove(&**client, "one").await,
        webhooks::rotate_secret(&**client, "one", None)
            .await
            .map(drop),
    ] {
        assert!(matches!(result, Err(DbError::Webhook(_))), "{result:?}");
    }
}

#[tokio::test]
async fn a_committed_insert_enqueues_one_job_per_matching_webhook() {
    let (db, registry) = setup().await;
    hook(&db, &registry, "a_both", "note", &["insert", "update"]).await;
    hook(&db, &registry, "b_insert", "note", &["insert"]).await;
    hook(&db, &registry, "c_update_only", "note", &["update"]).await;
    hook(&db, &registry, "d_other_doctype", "invoice", &["insert"]).await;
    hook(&db, &registry, "e_disabled", "note", &["insert"]).await;
    {
        let client = db.pool.get().await.unwrap();
        webhooks::set_enabled(&**client, "e_disabled", false)
            .await
            .unwrap();
    }
    let mut client = db.pool.get().await.unwrap();
    let stored = insert(
        &mut **client,
        &registry,
        "note",
        doc("hello"),
        &Caller::system("ann"),
    )
    .await
    .unwrap();
    assert_eq!(
        deliveries(&db).await,
        [
            format!("a_both/insert/note/{}/ann/queued", stored.name),
            format!("b_insert/insert/note/{}/ann/queued", stored.name),
        ]
    );
    // The payload: the document as the document API writes it, and a time.
    let rows = db
        .rows(
            "SELECT payload->'data'->>'title' || '|' || (payload->'data'->>'version') || '|' \
             || (payload->'data'->>'owner') || '|' || (payload->>'occurred_at') FROM fb_job \
             ORDER BY id LIMIT 1",
        )
        .await;
    let line = &rows[0][0];
    assert!(line.starts_with("hello|1|ann|20"), "{line}");
    let data = db
        .rows("SELECT (payload->'data')::text FROM fb_job ORDER BY id LIMIT 1")
        .await;
    let data: serde_json::Value = serde_json::from_str(&data[0][0]).unwrap();
    assert_eq!(data, forgebase_db::json::document_to_json(&stored).unwrap());
}

#[tokio::test]
async fn a_rolled_back_or_failed_operation_enqueues_nothing() {
    let (db, registry) = setup().await;
    hook(&db, &registry, "w", "note", &["insert", "update", "delete"]).await;
    let mut client = db.pool.get().await.unwrap();
    let ann = Caller::system("ann");

    // The caller's own transaction is rolled back after the insert: no job, no document.
    let raw: &mut forgebase_db::Client = &mut client;
    let mut tx = raw.transaction().await.unwrap();
    insert(&mut tx, &registry, "note", doc("gone"), &ann)
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    assert!(deliveries(&db).await.is_empty());
    assert_eq!(
        db.rows("SELECT count(*)::text FROM doc_note").await,
        [["0"]]
    );

    // A refused operation (a required field is missing) enqueues nothing.
    let invalid = Document::default();
    assert!(
        insert(&mut **client, &registry, "note", invalid, &ann)
            .await
            .is_err()
    );
    assert!(deliveries(&db).await.is_empty());

    // A failed update (stale version) enqueues nothing either; the insert before it did.
    let stored = insert(&mut **client, &registry, "note", doc("one"), &ann)
        .await
        .unwrap();
    assert!(
        update(
            &mut **client,
            &registry,
            "note",
            &stored.name,
            99,
            &title("x"),
            &ann
        )
        .await
        .is_err()
    );
    assert_eq!(deliveries(&db).await.len(), 1);

    // A committed caller transaction keeps its jobs: they are the operations' own.
    let raw: &mut forgebase_db::Client = &mut client;
    let mut tx = raw.transaction().await.unwrap();
    update(
        &mut tx,
        &registry,
        "note",
        &stored.name,
        1,
        &title("two"),
        &ann,
    )
    .await
    .unwrap();
    delete(&mut tx, &registry, "note", &stored.name, 2, &ann)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        deliveries(&db).await,
        [
            format!("w/insert/note/{}/ann/queued", stored.name),
            format!("w/update/note/{}/ann/queued", stored.name),
            format!("w/delete/note/{}/ann/queued", stored.name),
        ]
    );
}

#[tokio::test]
async fn every_operation_fires_its_events() {
    let (db, registry) = setup().await;
    let all = [
        "insert",
        "update",
        "delete",
        "submit",
        "cancel",
        "transition",
    ];
    hook(&db, &registry, "w", "invoice", &all).await;
    let mut client = db.pool.get().await.unwrap();
    let ann = Caller::system("ann");

    let one = insert(&mut **client, &registry, "invoice", doc("inv"), &ann)
        .await
        .unwrap();
    let two = update(
        &mut **client,
        &registry,
        "invoice",
        &one.name,
        1,
        &title("inv 2"),
        &ann,
    )
    .await
    .unwrap();
    submit(
        &mut **client,
        &registry,
        "invoice",
        &one.name,
        two.version,
        &ann,
    )
    .await
    .unwrap();
    cancel(&mut **client, &registry, "invoice", &one.name, 3, &ann)
        .await
        .unwrap();
    // An amendment is an insert of the new draft.
    let amended = amend(&mut **client, &registry, "invoice", &one.name, 4, &ann)
        .await
        .unwrap();
    delete(
        &mut **client,
        &registry,
        "invoice",
        &amended.name,
        amended.version,
        &ann,
    )
    .await
    .unwrap();
    let n = &one.name;
    assert_eq!(
        deliveries(&db).await,
        [
            format!("w/insert/invoice/{n}/ann/queued"),
            format!("w/update/invoice/{n}/ann/queued"),
            format!("w/submit/invoice/{n}/ann/queued"),
            format!("w/cancel/invoice/{n}/ann/queued"),
            format!("w/insert/invoice/{}/ann/queued", amended.name),
            format!("w/delete/invoice/{}/ann/queued", amended.name),
        ]
    );
    // The statuses in the snapshots: draft, draft, submitted, cancelled, draft (the amendment).
    let statuses = db
        .rows("SELECT payload->'data'->>'docstatus' FROM fb_job ORDER BY id")
        .await;
    assert_eq!(
        statuses
            .into_iter()
            .map(|r| r[0].clone())
            .collect::<Vec<_>>(),
        ["0", "0", "1", "2", "0", "0"]
    );
}

#[tokio::test]
async fn a_delete_sends_the_document_as_it_was_with_its_rows() {
    let (db, registry) = setup().await;
    hook(&db, &registry, "w", "invoice", &["delete"]).await;
    let mut client = db.pool.get().await.unwrap();
    let ann = Caller::system("ann");
    let mut with_rows = doc("inv");
    with_rows.tables.insert(
        "lines".to_owned(),
        vec![
            BTreeMap::from([("item".to_owned(), text("a"))]),
            BTreeMap::from([("item".to_owned(), text("b"))]),
        ],
    );
    let stored = insert(&mut **client, &registry, "invoice", with_rows, &ann)
        .await
        .unwrap();
    delete(&mut **client, &registry, "invoice", &stored.name, 1, &ann)
        .await
        .unwrap();
    let rows = db
        .rows("SELECT payload->'data'->>'title', payload->'data'->'lines'->1->>'item', payload->>'event' FROM fb_job")
        .await;
    assert_eq!(rows, [["inv", "b", "delete"]]);
}

#[tokio::test]
async fn a_transition_fires_transition_and_the_docstatus_event() {
    let (db, registry) = setup().await;
    hook(&db, &registry, "t", "order", &["transition"]).await;
    hook(&db, &registry, "s", "order", &["submit", "cancel"]).await;
    let mut client = db.pool.get().await.unwrap();
    let ann = Caller::system("ann");
    let order = insert(&mut **client, &registry, "order", doc("o"), &ann)
        .await
        .unwrap();
    // Draft -> Review: a transition only.
    let step = transition(
        &mut **client,
        &registry,
        "order",
        &order.name,
        "Send",
        1,
        &ann,
    )
    .await
    .unwrap();
    // Review -> Done: moves docstatus to 1.
    let step = transition(
        &mut **client,
        &registry,
        "order",
        &order.name,
        "Finish",
        step.version,
        &ann,
    )
    .await
    .unwrap();
    // Done -> Void: moves docstatus to 2.
    transition(
        &mut **client,
        &registry,
        "order",
        &order.name,
        "Revoke",
        step.version,
        &ann,
    )
    .await
    .unwrap();
    let n = &order.name;
    // One op after the other; within one op by webhook name (`s` before `t`), and for a webhook
    // the events in the order of the operation.
    assert_eq!(
        deliveries(&db).await,
        [
            format!("t/transition/order/{n}/ann/queued"),
            format!("s/submit/order/{n}/ann/queued"),
            format!("t/transition/order/{n}/ann/queued"),
            format!("s/cancel/order/{n}/ann/queued"),
            format!("t/transition/order/{n}/ann/queued"),
        ]
    );
    // The snapshot has the workflow state after the transition.
    let states = db
        .rows("SELECT payload->>'event' || ':' || (payload->'data'->>'workflow_state') FROM fb_job ORDER BY id")
        .await;
    assert_eq!(
        states.into_iter().map(|r| r[0].clone()).collect::<Vec<_>>(),
        [
            "transition:Review",
            "submit:Done",
            "transition:Done",
            "cancel:Void",
            "transition:Void"
        ]
    );
}

#[tokio::test]
async fn a_snapshot_over_the_job_payload_limit_is_sent_without_its_data() {
    let (db, registry) = setup().await;
    hook(&db, &registry, "w", "note", &["insert"]).await;
    let mut client = db.pool.get().await.unwrap();
    let mut big = doc("big");
    big.values
        .insert("body".to_owned(), text(&"x".repeat(70_000)));
    let stored = insert(
        &mut **client,
        &registry,
        "note",
        big,
        &Caller::system("ann"),
    )
    .await
    .expect("the write is not refused because of the webhook");
    let rows = db
        .rows("SELECT payload->>'truncated', coalesce(payload->>'data', 'null'), payload->>'name' FROM fb_job")
        .await;
    assert_eq!(rows, [["true".to_owned(), "null".into(), stored.name]]);
    assert_eq!(
        db.rows("SELECT count(*)::text FROM doc_note").await,
        [["1"]]
    );
}

#[tokio::test]
async fn a_binary_without_the_delivery_job_refuses_the_write_clearly() {
    let db = TestDb::migrated().await;
    // No `add_job(DELIVER_JOB)`: a registry that the framework did not build.
    let registry = registry(&[NOTE]);
    db.sync(&registry).await.expect("sync");
    let client = db.pool.get().await.unwrap();
    webhooks::add(
        &**client,
        &registry,
        &NewWebhook {
            name: "w",
            doctype: "note",
            events: &events(&["insert"]),
            url: "https://example.com/x",
            allow_private: false,
            insecure_http: false,
        },
        None,
    )
    .await
    .unwrap();
    drop(client);
    let mut client = db.pool.get().await.unwrap();
    let error = insert(
        &mut **client,
        &registry,
        "note",
        doc("x"),
        &Caller::system("ann"),
    )
    .await
    .expect_err("no delivery job");
    assert!(
        matches!(&error, DbError::Job(m) if m.contains("forgebase.deliver_webhook")),
        "{error}"
    );
    assert_eq!(
        db.rows("SELECT count(*)::text FROM doc_note").await,
        [["0"]]
    );
}

#[tokio::test]
async fn the_lookup_uses_the_partial_index() {
    let (db, registry) = setup().await;
    hook(&db, &registry, "w", "note", &["insert"]).await;
    let client = db.pool.get().await.unwrap();
    // The planner prefers a sequential scan on a table of one row; force the index to show that
    // the query can use it.
    client
        .batch_execute("SET enable_seqscan = off")
        .await
        .unwrap();
    let rows = client
        .query(
            "EXPLAIN SELECT name, events, now() FROM fb_webhook \
             WHERE enabled AND doctype = $1 AND events && $2::text[]",
            &[&"note", &vec!["insert"]],
        )
        .await
        .unwrap();
    let plan: Vec<String> = rows.iter().map(|r| r.get::<_, String>(0)).collect();
    assert!(
        plan.iter().any(|line| line.contains("fb_webhook_doctype")),
        "{plan:?}"
    );
}
