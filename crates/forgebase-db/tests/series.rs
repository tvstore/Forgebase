//! Naming series (M2.4) against a real PostgreSQL (see `common`): the counter lives in `fb_series`
//! and moves with the insert's transaction, so numbering has no gaps per prefix.

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;

use common::{TestDb, registry};
use forgebase_core::document::{Document, ValidationError};
use forgebase_core::hooks::{DocTypeHooks, HookContext, HookError};
use forgebase_core::meta::Registry;
use forgebase_core::value::Value;
use forgebase_db::{DbError, amend, cancel, delete, insert, submit};

const INVOICE: &str = r#"
name = "invoice"
label = "Invoice"
naming = { series = "INV-.YYYY.-.#####" }
[[fields]]
name = "ref"
type = "data"
unique = true
"#;

/// The same prefix as `INVOICE` with another width: they share a counter.
const BILL: &str = r#"
name = "bill"
label = "Bill"
naming = { series = "INV-.YYYY.-.#######.-.MM" }
"#;

const TICKET: &str = r#"
name = "ticket"
label = "Ticket"
naming = { series = "T-.YY.MM.DD.-.###" }
"#;

const ORDER: &str = r#"
name = "order"
label = "Order"
submittable = true
naming = { series = "SO-.####" }
"#;

fn doc(reference: Option<&str>) -> Document {
    Document {
        name: None,
        values: reference
            .map(|r| BTreeMap::from([("ref".to_owned(), Value::Text(r.to_owned()))]))
            .unwrap_or_default(),
        tables: BTreeMap::new(),
    }
}

async fn setup(texts: &[&str]) -> (TestDb, Registry) {
    let db = TestDb::migrated().await;
    let reg = registry(texts);
    db.sync(&reg).await.expect("sync");
    (db, reg)
}

/// The UTC date of the database, as `YYYY`, `YY`, `MM`, `DD`.
async fn utc(db: &TestDb) -> [String; 4] {
    let row = db
        .rows(
            "SELECT to_char(now() AT TIME ZONE 'UTC', 'YYYY'), to_char(now() AT TIME ZONE 'UTC', 'YY'), \
             to_char(now() AT TIME ZONE 'UTC', 'MM'), to_char(now() AT TIME ZONE 'UTC', 'DD')",
        )
        .await
        .remove(0);
    [
        row[0].clone(),
        row[1].clone(),
        row[2].clone(),
        row[3].clone(),
    ]
}

async fn counters(db: &TestDb) -> Vec<(String, String)> {
    db.rows("SELECT prefix, current::text FROM fb_series ORDER BY prefix")
        .await
        .into_iter()
        .map(|r| (r[0].clone(), r[1].clone()))
        .collect()
}

#[tokio::test]
async fn names_come_from_the_series_with_the_utc_date_of_the_transaction() {
    let (db, reg) = setup(&[INVOICE, TICKET]).await;
    let [yyyy, yy, mm, dd] = utc(&db).await;
    let mut client = db.pool.get().await.unwrap();
    let c = &mut **client;
    for n in 1..=3 {
        let stored = insert(c, &reg, "invoice", doc(None), "ann").await.unwrap();
        assert_eq!(stored.name, format!("INV-{yyyy}-{n:05}"));
        assert_eq!(stored.version, 1);
    }
    let ticket = insert(c, &reg, "ticket", Document::default(), "ann")
        .await
        .unwrap();
    assert_eq!(ticket.name, format!("T-{yy}{mm}{dd}-001"));
    // One counter row per prefix, holding the last number.
    assert_eq!(
        counters(&db).await,
        [
            (format!("INV-{yyyy}-"), "3".to_owned()),
            (format!("T-{yy}{mm}{dd}-"), "1".to_owned())
        ]
    );
}

#[tokio::test]
async fn the_date_is_the_utc_date_whatever_the_session_time_zone() {
    let (db, reg) = setup(&[TICKET]).await;
    let [_, yy, mm, dd] = utc(&db).await;
    let client = db.pool.get().await.unwrap();
    // A zone 14 hours ahead of UTC: its local date differs from the UTC date for a third of
    // the day; the name must not follow it.
    client
        .batch_execute("SET TIME ZONE 'Pacific/Kiritimati'")
        .await
        .unwrap();
    let mut client = client;
    let stored = insert(&mut **client, &reg, "ticket", Document::default(), "ann")
        .await
        .unwrap();
    assert_eq!(stored.name, format!("T-{yy}{mm}{dd}-001"));
}

#[tokio::test]
async fn patterns_with_the_same_prefix_share_one_counter() {
    let (db, reg) = setup(&[INVOICE, BILL]).await;
    let [yyyy, _, mm, _] = utc(&db).await;
    let mut client = db.pool.get().await.unwrap();
    let c = &mut **client;
    let a = insert(c, &reg, "invoice", doc(None), "ann").await.unwrap();
    let b = insert(c, &reg, "bill", Document::default(), "ann")
        .await
        .unwrap();
    let a2 = insert(c, &reg, "invoice", doc(None), "ann").await.unwrap();
    assert_eq!(a.name, format!("INV-{yyyy}-00001"));
    // The other width and the date part after the number; the number goes on counting.
    assert_eq!(b.name, format!("INV-{yyyy}-0000002-{mm}"));
    assert_eq!(a2.name, format!("INV-{yyyy}-00003"));
    assert_eq!(
        counters(&db).await,
        [(format!("INV-{yyyy}-"), "3".to_owned())]
    );
}

#[tokio::test]
async fn a_number_wider_than_the_hashes_is_printed_in_full() {
    let (db, reg) = setup(&[ORDER]).await;
    db.exec("INSERT INTO fb_series (prefix, current) VALUES ('SO-', 9999)")
        .await;
    let mut client = db.pool.get().await.unwrap();
    let stored = insert(&mut **client, &reg, "order", Document::default(), "ann")
        .await
        .unwrap();
    assert_eq!(stored.name, "SO-10000");
}

#[tokio::test]
async fn a_name_cannot_be_given_with_a_series() {
    let (db, reg) = setup(&[INVOICE]).await;
    let mut client = db.pool.get().await.unwrap();
    let mut given = doc(None);
    given.name = Some("INV-1".to_owned());
    let error = insert(&mut **client, &reg, "invoice", given, "ann")
        .await
        .unwrap_err();
    assert!(
        matches!(&error, DbError::Naming(m) if m.contains("naming series")),
        "{error}"
    );
    assert!(counters(&db).await.is_empty(), "no number was taken");
}

#[tokio::test]
async fn a_failed_insert_gives_its_number_back() {
    let (db, reg) = setup(&[INVOICE]).await;
    let [yyyy, ..] = utc(&db).await;
    let mut client = db.pool.get().await.unwrap();
    let c = &mut **client;
    let first = insert(c, &reg, "invoice", doc(Some("dup")), "ann")
        .await
        .unwrap();
    assert_eq!(first.name, format!("INV-{yyyy}-00001"));

    // The unique violation happens after the number was taken, in the same transaction.
    let error = insert(c, &reg, "invoice", doc(Some("dup")), "ann")
        .await
        .unwrap_err();
    assert!(matches!(error, DbError::UniqueViolation { .. }), "{error}");
    assert_eq!(
        counters(&db).await,
        [(format!("INV-{yyyy}-"), "1".to_owned())]
    );

    let next = insert(c, &reg, "invoice", doc(Some("other")), "ann")
        .await
        .unwrap();
    assert_eq!(
        next.name,
        format!("INV-{yyyy}-00002"),
        "the number is reused, no gap"
    );
}

struct Refuse;

impl DocTypeHooks for Refuse {
    fn validate(&self, _ctx: &HookContext<'_>, _doc: &mut Document) -> Result<(), HookError> {
        Err(HookError::new("no"))
    }
}

#[tokio::test]
async fn documents_refused_before_naming_take_no_number() {
    let (db, _) = setup(&[INVOICE]).await;
    let mut hooked = registry(&[INVOICE]);
    hooked.add_hooks("invoice", Arc::new(Refuse)).unwrap();
    let plain = registry(&[INVOICE]);
    let mut client = db.pool.get().await.unwrap();
    let c = &mut **client;
    // A hook error, and a validation error (a value for an unknown field).
    assert!(matches!(
        insert(c, &hooked, "invoice", doc(None), "ann").await,
        Err(DbError::Hook { .. })
    ));
    let mut invalid = doc(None);
    invalid.values.insert("nope".to_owned(), Value::Int(1));
    let error = insert(c, &plain, "invoice", invalid, "ann")
        .await
        .unwrap_err();
    assert!(
        matches!(&error, DbError::Validation(e) if e == &[ValidationError::UnknownField { field: "nope".into() }]),
        "{error}"
    );
    assert!(counters(&db).await.is_empty());
    let first = insert(c, &plain, "invoice", doc(None), "ann")
        .await
        .unwrap();
    assert!(first.name.ends_with("-00001"), "{}", first.name);
}

#[tokio::test]
async fn the_counter_moves_with_a_callers_transaction() {
    let (db, reg) = setup(&[INVOICE]).await;
    let mut client = db.pool.get().await.unwrap();
    let raw: &mut forgebase_db::Client = &mut client;
    let mut tx = raw.transaction().await.unwrap();
    let a = insert(&mut tx, &reg, "invoice", doc(None), "ann")
        .await
        .unwrap();
    assert!(a.name.ends_with("-00001"));
    // A failed call inside the transaction (a savepoint) gives only its own number back.
    let mut bad = doc(None);
    bad.name = Some("x".to_owned());
    assert!(insert(&mut tx, &reg, "invoice", bad, "ann").await.is_err());
    let b = insert(&mut tx, &reg, "invoice", doc(None), "ann")
        .await
        .unwrap();
    assert!(b.name.ends_with("-00002"), "{}", b.name);
    tx.rollback().await.unwrap();
    // Everything of the transaction is gone, the counter row too.
    assert!(counters(&db).await.is_empty());
    let c = insert(raw, &reg, "invoice", doc(None), "ann")
        .await
        .unwrap();
    assert!(c.name.ends_with("-00001"), "{}", c.name);
}

#[tokio::test]
async fn twenty_parallel_inserts_get_the_numbers_one_to_twenty() {
    let (db, reg) = setup(&[INVOICE]).await;
    let [yyyy, ..] = utc(&db).await;
    let mut tasks = Vec::new();
    for i in 0..20 {
        let (pool, reg) = (db.pool.clone(), reg.clone());
        tasks.push(tokio::spawn(async move {
            let mut client = pool.get().await.unwrap();
            insert(
                &mut **client,
                &reg,
                "invoice",
                doc(Some(&format!("r{i}"))),
                "ann",
            )
            .await
            .unwrap()
            .name
        }));
    }
    let mut names = Vec::new();
    for task in tasks {
        names.push(task.await.unwrap());
    }
    names.sort();
    let expected: Vec<String> = (1..=20).map(|n| format!("INV-{yyyy}-{n:05}")).collect();
    assert_eq!(names, expected, "no gap and no duplicate");
    assert_eq!(
        counters(&db).await,
        [(format!("INV-{yyyy}-"), "20".to_owned())]
    );
}

#[tokio::test]
async fn parallel_inserts_with_failures_leave_no_gap() {
    let (db, reg) = setup(&[INVOICE]).await;
    let mut tasks = Vec::new();
    // Ten inserts want the same unique value: one wins, nine fail after taking a number.
    for i in 0..10 {
        let (pool, reg) = (db.pool.clone(), reg.clone());
        tasks.push(tokio::spawn(async move {
            let mut client = pool.get().await.unwrap();
            insert(&mut **client, &reg, "invoice", doc(Some("same")), "ann")
                .await
                .map(|d| (i, d.name))
        }));
    }
    let mut winners = 0;
    for task in tasks {
        match task.await.unwrap() {
            Ok((_, name)) => {
                winners += 1;
                assert!(name.ends_with("-00001"), "{name}");
            }
            Err(DbError::UniqueViolation { .. }) => {}
            Err(other) => panic!("{other}"),
        }
    }
    assert_eq!(winners, 1);
    let mut client = db.pool.get().await.unwrap();
    let next = insert(&mut **client, &reg, "invoice", doc(Some("next")), "ann")
        .await
        .unwrap();
    assert!(next.name.ends_with("-00002"), "{}", next.name);
}

#[tokio::test]
async fn a_submittable_series_doctype_amends_by_its_own_rule_and_takes_no_number() {
    let (db, reg) = setup(&[ORDER]).await;
    let mut client = db.pool.get().await.unwrap();
    let c = &mut **client;
    let one = insert(c, &reg, "order", Document::default(), "ann")
        .await
        .unwrap();
    assert_eq!(one.name, "SO-0001");
    let one = submit(c, &reg, "order", &one.name, 1, "ann").await.unwrap();
    let one = cancel(c, &reg, "order", &one.name, 2, "ann").await.unwrap();
    let amended = amend(c, &reg, "order", &one.name, 3, "ann").await.unwrap();
    assert_eq!(amended.name, "SO-0001-1");
    assert_eq!(amended.amended_from.as_deref(), Some("SO-0001"));
    assert_eq!(counters(&db).await, [("SO-".to_owned(), "1".to_owned())]);
    // The next new order is numbered 2, and its amendments follow the same rule.
    let two = insert(c, &reg, "order", Document::default(), "ann")
        .await
        .unwrap();
    assert_eq!(two.name, "SO-0002");
    let _ = amended;
    // Deleting a document does not give its number back (only a rollback does).
    delete(c, &reg, "order", &two.name, 1, "ann").await.unwrap();
    let three = insert(c, &reg, "order", Document::default(), "ann")
        .await
        .unwrap();
    assert_eq!(three.name, "SO-0003");
}

#[tokio::test]
async fn the_series_prefix_is_bound_never_interpolated() {
    // A literal that would break out of a statement is stored as plain text.
    let evil = r#"
name = "evil"
label = "Evil"
naming = { series = "x'');DROP TABLE fb_series;--.#" }
"#;
    let (db, reg) = setup(&[evil]).await;
    let mut client = db.pool.get().await.unwrap();
    let stored = insert(&mut **client, &reg, "evil", Document::default(), "ann")
        .await
        .unwrap();
    assert_eq!(stored.name, "x'');DROP TABLE fb_series;--1");
    assert_eq!(
        counters(&db).await,
        [("x'');DROP TABLE fb_series;--".to_owned(), "1".to_owned())]
    );
}
