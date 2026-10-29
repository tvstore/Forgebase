//! Optimistic concurrency: `expected_version` on update and delete (see `common`).

mod common;

use std::collections::BTreeMap;
use std::time::Duration;

use common::{CUSTOMER, TestDb, registry};
use forgebase_core::document::{Changes, Document};
use forgebase_core::meta::{DocType, Registry};
use forgebase_core::permission::Caller;
use forgebase_core::value::Value;
use forgebase_db::{DbError, delete, get, insert, update};

async fn setup() -> (TestDb, Registry, DocType) {
    let db = TestDb::migrated().await;
    let registry = registry(&[CUSTOMER]);
    db.sync(&registry).await.expect("sync");
    let customer = registry.get("customer").expect("customer").clone();
    (db, registry, customer)
}

fn customer(email: &str) -> Document {
    Document {
        name: None,
        values: BTreeMap::from([("email".to_owned(), Value::Text(email.to_owned()))]),
        tables: BTreeMap::new(),
    }
}

fn points(n: i64) -> Changes {
    Changes {
        values: BTreeMap::from([("points".to_owned(), Value::Int(n))]),
        tables: BTreeMap::new(),
    }
}

#[tokio::test]
async fn update_with_a_stale_version_is_a_conflict_and_changes_nothing() {
    let (db, reg, customer_type) = setup().await;
    let mut client = db.pool.get().await.unwrap();
    let created = insert(
        &mut **client,
        &reg,
        &customer_type.name,
        customer("a@x.y"),
        &Caller::system("cli"),
    )
    .await
    .unwrap();
    let v2 = update(
        &mut **client,
        &reg,
        &customer_type.name,
        &created.name,
        1,
        &points(1),
        &Caller::system("cli"),
    )
    .await
    .unwrap();
    assert_eq!(v2.version, 2);

    // Reusing the old version fails and reports the current one.
    let error = update(
        &mut **client,
        &reg,
        &customer_type.name,
        &created.name,
        1,
        &points(2),
        &Caller::system("cli"),
    )
    .await
    .unwrap_err();
    let DbError::Conflict {
        doctype,
        name,
        expected,
        current,
    } = &error
    else {
        panic!("expected Conflict, got {error}");
    };
    assert_eq!(
        (doctype.as_str(), name.as_str(), *expected, *current),
        ("customer", created.name.as_str(), 1, 2)
    );
    assert_eq!(
        error.to_string(),
        format!(
            "customer `{}` was changed by someone else: expected version 1, current version 2",
            created.name
        )
    );
    // A version from the future is a conflict too.
    let error = update(
        &mut **client,
        &reg,
        &customer_type.name,
        &created.name,
        9,
        &points(2),
        &Caller::system("cli"),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(
            error,
            DbError::Conflict {
                expected: 9,
                current: 2,
                ..
            }
        ),
        "{error}"
    );
    let unchanged = get(
        &mut **client,
        &reg,
        &customer_type.name,
        &created.name,
        &Caller::system("cli"),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(unchanged, v2);
}

#[tokio::test]
async fn delete_with_a_stale_version_is_a_conflict_and_deletes_nothing() {
    let (db, reg, customer_type) = setup().await;
    let mut client = db.pool.get().await.unwrap();
    let created = insert(
        &mut **client,
        &reg,
        &customer_type.name,
        customer("d@x.y"),
        &Caller::system("cli"),
    )
    .await
    .unwrap();
    update(
        &mut **client,
        &reg,
        &customer_type.name,
        &created.name,
        1,
        &points(1),
        &Caller::system("cli"),
    )
    .await
    .unwrap();
    let error = delete(
        &mut **client,
        &reg,
        &customer_type.name,
        &created.name,
        1,
        &Caller::system("ann"),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(
            error,
            DbError::Conflict {
                expected: 1,
                current: 2,
                ..
            }
        ),
        "{error}"
    );
    assert!(
        get(
            &mut **client,
            &reg,
            &customer_type.name,
            &created.name,
            &Caller::system("cli")
        )
        .await
        .unwrap()
        .is_some()
    );
    delete(
        &mut **client,
        &reg,
        &customer_type.name,
        &created.name,
        2,
        &Caller::system("ann"),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn a_missing_document_is_not_found_not_a_conflict() {
    let (db, reg, customer_type) = setup().await;
    let mut client = db.pool.get().await.unwrap();
    let error = update(
        &mut **client,
        &reg,
        &customer_type.name,
        "nobody",
        1,
        &points(1),
        &Caller::system("cli"),
    )
    .await
    .unwrap_err();
    assert!(matches!(error, DbError::NotFound { .. }), "{error}");
    let error = delete(
        &mut **client,
        &reg,
        &customer_type.name,
        "nobody",
        1,
        &Caller::system("ann"),
    )
    .await
    .unwrap_err();
    assert!(matches!(error, DbError::NotFound { .. }), "{error}");
}

#[tokio::test]
async fn a_conflict_inside_a_transaction_does_not_abort_it() {
    let (db, reg, customer_type) = setup().await;
    let mut client = db.pool.get().await.unwrap();
    let raw: &mut forgebase_db::Client = &mut client;
    let mut tx = raw.transaction().await.unwrap();
    let created = insert(
        &mut tx,
        &reg,
        &customer_type.name,
        customer("t@x.y"),
        &Caller::system("cli"),
    )
    .await
    .unwrap();
    // The failed update is a plain no-row result, so the transaction stays usable.
    let error = update(
        &mut tx,
        &reg,
        &customer_type.name,
        &created.name,
        5,
        &points(1),
        &Caller::system("cli"),
    )
    .await
    .unwrap_err();
    assert!(matches!(error, DbError::Conflict { .. }), "{error}");
    update(
        &mut tx,
        &reg,
        &customer_type.name,
        &created.name,
        1,
        &points(1),
        &Caller::system("cli"),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
}

#[tokio::test]
async fn a_second_writer_waits_then_gets_a_conflict() {
    let (db, reg, customer_type) = setup().await;
    let mut first = db.pool.get().await.unwrap();
    let mut second = db.pool.get().await.unwrap();
    let created = insert(
        &mut **first,
        &reg,
        &customer_type.name,
        customer("c@x.y"),
        &Caller::system("cli"),
    )
    .await
    .unwrap();

    // The first writer updates inside a transaction and has not committed yet.
    let mut first = first;
    let raw: &mut forgebase_db::Client = &mut first;
    let mut tx = raw.transaction().await.unwrap();
    update(
        &mut tx,
        &reg,
        &customer_type.name,
        &created.name,
        1,
        &points(1),
        &Caller::system("one"),
    )
    .await
    .unwrap();

    // The second writer, holding the same version, blocks on the row lock ...
    let two = points(2);
    let caller_two = Caller::system("two");
    let error = {
        let late = update(
            &mut **second,
            &reg,
            &customer_type.name,
            &created.name,
            1,
            &two,
            &caller_two,
        );
        tokio::pin!(late);
        assert!(
            tokio::time::timeout(Duration::from_millis(300), &mut late)
                .await
                .is_err(),
            "the second update should wait for the first transaction"
        );
        // ... and once the first commits, the version no longer matches.
        tx.commit().await.unwrap();
        late.await.unwrap_err()
    };
    assert!(
        matches!(
            error,
            DbError::Conflict {
                expected: 1,
                current: 2,
                ..
            }
        ),
        "{error}"
    );
    let stored = get(
        &mut **second,
        &reg,
        &customer_type.name,
        &created.name,
        &Caller::system("cli"),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(stored.version, 2);
    assert_eq!(stored.modified_by, "one");
    assert_eq!(stored.values["points"], Value::Int(1));
}

#[tokio::test]
async fn racing_updates_with_the_same_version_succeed_exactly_once() {
    let (db, reg, customer_type) = setup().await;
    for round in 0..10 {
        let email = format!("race{round}@x.y");
        let created = {
            let mut client = db.pool.get().await.unwrap();
            insert(
                &mut **client,
                &reg,
                &customer_type.name,
                customer(&email),
                &Caller::system("cli"),
            )
            .await
            .unwrap()
        };
        let (mut a, mut b) = (db.pool.get().await.unwrap(), db.pool.get().await.unwrap());
        let (one, two) = (points(1), points(2));
        let (caller_a, caller_b) = (Caller::system("a"), Caller::system("b"));
        let (ra, rb) = tokio::join!(
            update(
                &mut **a,
                &reg,
                &customer_type.name,
                &created.name,
                1,
                &one,
                &caller_a
            ),
            update(
                &mut **b,
                &reg,
                &customer_type.name,
                &created.name,
                1,
                &two,
                &caller_b
            ),
        );
        let outcomes = [ra, rb];
        let ok = outcomes.iter().filter(|r| r.is_ok()).count();
        let conflicts = outcomes
            .iter()
            .filter(|r| matches!(r, Err(DbError::Conflict { current: 2, .. })))
            .count();
        assert_eq!((ok, conflicts), (1, 1), "round {round}: {outcomes:?}");
        let stored = get(
            &mut **a,
            &reg,
            &customer_type.name,
            &created.name,
            &Caller::system("cli"),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(stored.version, 2, "round {round}");
    }
}

#[tokio::test]
async fn racing_delete_and_update_with_the_same_version_succeed_exactly_once() {
    let (db, reg, customer_type) = setup().await;
    let created = {
        let mut client = db.pool.get().await.unwrap();
        insert(
            &mut **client,
            &reg,
            &customer_type.name,
            customer("dr@x.y"),
            &Caller::system("cli"),
        )
        .await
        .unwrap()
    };
    let (mut a, mut b) = (db.pool.get().await.unwrap(), db.pool.get().await.unwrap());
    let one = points(1);
    let (caller_a, caller_ann) = (Caller::system("a"), Caller::system("ann"));
    let (updated, deleted) = tokio::join!(
        update(
            &mut **a,
            &reg,
            &customer_type.name,
            &created.name,
            1,
            &one,
            &caller_a
        ),
        delete(
            &mut **b,
            &reg,
            &customer_type.name,
            &created.name,
            1,
            &caller_ann
        ),
    );
    // Either the update won (the delete then sees version 2) or the delete won (the update then
    // finds nothing); never both.
    match (&updated, &deleted) {
        (Ok(_), Err(DbError::Conflict { current: 2, .. }))
        | (Err(DbError::NotFound { .. }), Ok(())) => {}
        other => panic!("unexpected outcome: {other:?}"),
    }
}
