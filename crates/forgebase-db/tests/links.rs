//! Link fields: resolution of names, errors, foreign key behavior (see `common`).

mod common;

use std::collections::BTreeMap;
use std::time::Duration;

use common::{PERSON, SALES_ORDER, TestDb, registry};
use forgebase_core::document::{Changes, Document, ValidationError};
use forgebase_core::meta::{DocType, Registry};
use forgebase_core::permission::Caller;
use forgebase_core::value::Value;
use forgebase_db::{DbError, delete, get, insert, update};

async fn setup() -> (TestDb, Registry, DocType, DocType) {
    let db = TestDb::migrated().await;
    let registry = registry(&[PERSON, SALES_ORDER]);
    db.sync(&registry).await.expect("sync");
    let person = registry.get("person").expect("person").clone();
    let order = registry.get("sales_order").expect("order").clone();
    (db, registry, person, order)
}

fn values_of<const N: usize>(pairs: [(String, Value); N]) -> Changes {
    Changes {
        values: BTreeMap::from(pairs),
        tables: BTreeMap::new(),
    }
}

fn text(s: &str) -> Value {
    Value::Text(s.to_owned())
}

fn doc(values: &[(&str, Value)]) -> Document {
    Document {
        name: None,
        values: values
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.clone()))
            .collect(),
        tables: BTreeMap::new(),
    }
}

fn person(email: &str) -> Document {
    doc(&[("email", text(email))])
}

fn order(fields: &[(&str, &str)]) -> Document {
    let mut d = doc(&[("approver", text("boss@x.y"))]);
    for (k, v) in fields {
        d.values.insert((*k).to_owned(), text(v));
    }
    d
}

async fn count(db: &TestDb, table: &str) -> i64 {
    db.rows(&format!("SELECT count(*)::text FROM {table}"))
        .await[0][0]
        .parse()
        .expect("count")
}

#[tokio::test]
async fn link_values_are_names_in_the_api_and_ids_in_the_table() {
    let (db, reg, person_type, order_type) = setup().await;
    let mut client = db.pool.get().await.unwrap();
    let ann = insert(
        &mut **client,
        &reg,
        &person_type.name,
        person("ann@x.y"),
        &Caller::system("cli"),
    )
    .await
    .unwrap();
    insert(
        &mut **client,
        &reg,
        &person_type.name,
        person("boss@x.y"),
        &Caller::system("cli"),
    )
    .await
    .unwrap();
    let created = insert(
        &mut **client,
        &reg,
        &order_type.name,
        order(&[("customer", "ann@x.y"), ("title", "T")]),
        &Caller::system("cli"),
    )
    .await
    .unwrap();
    assert_eq!(created.values["customer"], text("ann@x.y"));
    assert_eq!(created.values["approver"], text("boss@x.y"));
    assert_eq!(created.values["follows"], Value::Null);

    let fetched = get(
        &mut **client,
        &reg,
        &order_type.name,
        &created.name,
        &Caller::system("cli"),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(fetched, created);

    // The table holds the target's id, not its name.
    let stored = db
        .rows("SELECT customer::text, pg_typeof(customer)::text FROM doc_sales_order")
        .await;
    assert_eq!(
        stored,
        vec![vec![ann.id.hyphenated().to_string(), "uuid".to_owned()]]
    );
}

#[tokio::test]
async fn missing_target_is_a_validation_error_and_nothing_is_written() {
    let (db, reg, person_type, order_type) = setup().await;
    let mut client = db.pool.get().await.unwrap();
    insert(
        &mut **client,
        &reg,
        &person_type.name,
        person("boss@x.y"),
        &Caller::system("cli"),
    )
    .await
    .unwrap();

    let error = insert(
        &mut **client,
        &reg,
        &order_type.name,
        order(&[("customer", "nobody@x.y")]),
        &Caller::system("cli"),
    )
    .await
    .unwrap_err();
    let DbError::Validation(errors) = &error else {
        panic!("expected validation error, got {error}");
    };
    assert_eq!(
        errors,
        &[ValidationError::LinkNotFound {
            field: "customer".into(),
            target: "person".into(),
            name: "nobody@x.y".into()
        }]
    );
    assert_eq!(
        error.to_string(),
        "validation failed: field `customer`: person `nobody@x.y` does not exist"
    );
    assert_eq!(count(&db, "doc_sales_order").await, 0);

    // Link errors are collected with the other problems, one per field, in one pass.
    let mut bad = doc(&[
        ("customer", text("nobody@x.y")),
        ("follows", text("no-such-order")),
        ("bogus", Value::Int(1)),
    ]);
    bad.values.insert("approver".into(), text("missing@x.y"));
    let error = insert(
        &mut **client,
        &reg,
        &order_type.name,
        bad,
        &Caller::system("cli"),
    )
    .await
    .unwrap_err();
    let DbError::Validation(errors) = &error else {
        panic!("{error}");
    };
    assert_eq!(errors.len(), 4, "{errors:?}");
    assert!(
        errors
            .iter()
            .any(|e| matches!(e, ValidationError::UnknownField { .. }))
    );
    assert_eq!(count(&db, "doc_sales_order").await, 0);
}

#[tokio::test]
async fn values_that_failed_validation_are_not_looked_up() {
    let (db, reg, person_type, order_type) = setup().await;
    let mut client = db.pool.get().await.unwrap();
    insert(
        &mut **client,
        &reg,
        &person_type.name,
        person("boss@x.y"),
        &Caller::system("cli"),
    )
    .await
    .unwrap();
    // NUL cannot be sent to PostgreSQL; the validation error is reported and no query is made.
    let error = insert(
        &mut **client,
        &reg,
        &order_type.name,
        order(&[("customer", "a\0b")]),
        &Caller::system("cli"),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(&error, DbError::Validation(e)
            if matches!(e.as_slice(), [ValidationError::ContainsNul { .. }])),
        "{error}"
    );
    // A blank required link is "required", not "not found".
    let error = insert(
        &mut **client,
        &reg,
        &order_type.name,
        doc(&[("approver", text("  "))]),
        &Caller::system("cli"),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(&error, DbError::Validation(e)
            if matches!(e.as_slice(), [ValidationError::Required { field }] if field == "approver")),
        "{error}"
    );
    // A required link may not be missing, null or cleared.
    let error = insert(
        &mut **client,
        &reg,
        &order_type.name,
        doc(&[]),
        &Caller::system("cli"),
    )
    .await
    .unwrap_err();
    assert!(matches!(error, DbError::Validation(_)), "{error}");
    let created = insert(
        &mut **client,
        &reg,
        &order_type.name,
        order(&[]),
        &Caller::system("cli"),
    )
    .await
    .unwrap();
    let changes = values_of([("approver".to_owned(), Value::Null)]);
    let error = update(
        &mut **client,
        &reg,
        &order_type.name,
        &created.name,
        1,
        &changes,
        &Caller::system("cli"),
    )
    .await
    .unwrap_err();
    assert!(matches!(error, DbError::Validation(_)), "{error}");
}

#[tokio::test]
async fn optional_links_may_be_null_or_absent() {
    let (db, reg, person_type, order_type) = setup().await;
    let mut client = db.pool.get().await.unwrap();
    insert(
        &mut **client,
        &reg,
        &person_type.name,
        person("boss@x.y"),
        &Caller::system("cli"),
    )
    .await
    .unwrap();
    let mut with_null = order(&[]);
    with_null.values.insert("customer".into(), Value::Null);
    let created = insert(
        &mut **client,
        &reg,
        &order_type.name,
        with_null,
        &Caller::system("cli"),
    )
    .await
    .unwrap();
    assert_eq!(created.values["customer"], Value::Null);
    assert_eq!(created.values["follows"], Value::Null);
    assert_eq!(
        db.rows("SELECT (customer IS NULL AND follows IS NULL)::text FROM doc_sales_order")
            .await,
        vec![vec!["true".to_owned()]]
    );
}

#[tokio::test]
async fn updating_links_resolves_and_validates_names() {
    let (db, reg, person_type, order_type) = setup().await;
    let mut client = db.pool.get().await.unwrap();
    for email in ["boss@x.y", "ann@x.y", "bob@x.y"] {
        insert(
            &mut **client,
            &reg,
            &person_type.name,
            person(email),
            &Caller::system("cli"),
        )
        .await
        .unwrap();
    }
    let created = insert(
        &mut **client,
        &reg,
        &order_type.name,
        order(&[("customer", "ann@x.y")]),
        &Caller::system("cli"),
    )
    .await
    .unwrap();

    let to_bob = values_of([("customer".to_owned(), text("bob@x.y"))]);
    let updated = update(
        &mut **client,
        &reg,
        &order_type.name,
        &created.name,
        1,
        &to_bob,
        &Caller::system("cli"),
    )
    .await
    .unwrap();
    assert_eq!(updated.version, 2);
    assert_eq!(updated.values["customer"], text("bob@x.y"));

    let missing = values_of([("customer".to_owned(), text("nobody@x.y"))]);
    let error = update(
        &mut **client,
        &reg,
        &order_type.name,
        &created.name,
        2,
        &missing,
        &Caller::system("cli"),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(&error, DbError::Validation(e)
            if matches!(e.as_slice(), [ValidationError::LinkNotFound { .. }])),
        "{error}"
    );
    let unchanged = get(
        &mut **client,
        &reg,
        &order_type.name,
        &created.name,
        &Caller::system("cli"),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(unchanged, updated);

    let cleared = values_of([("customer".to_owned(), Value::Null)]);
    let updated = update(
        &mut **client,
        &reg,
        &order_type.name,
        &created.name,
        2,
        &cleared,
        &Caller::system("cli"),
    )
    .await
    .unwrap();
    assert_eq!(updated.values["customer"], Value::Null);
    assert_eq!(updated.version, 3);
}

#[tokio::test]
async fn self_links_work() {
    let (db, reg, person_type, order_type) = setup().await;
    let mut client = db.pool.get().await.unwrap();
    insert(
        &mut **client,
        &reg,
        &person_type.name,
        person("boss@x.y"),
        &Caller::system("cli"),
    )
    .await
    .unwrap();
    let first = insert(
        &mut **client,
        &reg,
        &order_type.name,
        order(&[]),
        &Caller::system("cli"),
    )
    .await
    .unwrap();
    let second = insert(
        &mut **client,
        &reg,
        &order_type.name,
        order(&[("follows", &first.name)]),
        &Caller::system("cli"),
    )
    .await
    .unwrap();
    assert_eq!(second.values["follows"], text(&first.name));
    // An update may point a document at itself.
    let changes = values_of([("follows".to_owned(), text(&first.name))]);
    let updated = update(
        &mut **client,
        &reg,
        &order_type.name,
        &first.name,
        1,
        &changes,
        &Caller::system("cli"),
    )
    .await
    .unwrap();
    assert_eq!(updated.values["follows"], text(&first.name));
}

#[tokio::test]
async fn deleting_a_linked_document_names_the_linking_doctype() {
    let (db, reg, person_type, order_type) = setup().await;
    let mut client = db.pool.get().await.unwrap();
    let boss = insert(
        &mut **client,
        &reg,
        &person_type.name,
        person("boss@x.y"),
        &Caller::system("cli"),
    )
    .await
    .unwrap();
    let created = insert(
        &mut **client,
        &reg,
        &order_type.name,
        order(&[]),
        &Caller::system("cli"),
    )
    .await
    .unwrap();

    let error = delete(
        &mut **client,
        &reg,
        &person_type.name,
        &boss.name,
        1,
        &Caller::system("ann"),
    )
    .await
    .unwrap_err();
    let DbError::LinkedDocumentExists {
        doctype,
        name,
        referenced_by,
    } = &error
    else {
        panic!("expected LinkedDocumentExists, got {error}");
    };
    assert_eq!(
        (doctype.as_str(), name.as_str(), referenced_by.as_deref()),
        ("person", "boss@x.y", Some("sales_order"))
    );
    assert_eq!(
        error.to_string(),
        "person `boss@x.y` cannot be deleted: it is linked from sales_order"
    );
    assert_eq!(count(&db, "doc_person").await, 1);

    // A stale version is reported as a conflict before anything is attempted.
    let error = delete(
        &mut **client,
        &reg,
        &person_type.name,
        &boss.name,
        7,
        &Caller::system("ann"),
    )
    .await
    .unwrap_err();
    assert!(matches!(error, DbError::Conflict { .. }), "{error}");

    // Deleting the linking document first releases the target.
    delete(
        &mut **client,
        &reg,
        &order_type.name,
        &created.name,
        1,
        &Caller::system("ann"),
    )
    .await
    .unwrap();
    delete(
        &mut **client,
        &reg,
        &person_type.name,
        &boss.name,
        1,
        &Caller::system("ann"),
    )
    .await
    .unwrap();
    assert_eq!(count(&db, "doc_person").await, 0);
}

#[tokio::test]
async fn a_linked_delete_in_a_transaction_fails_cleanly_and_leaves_it_usable() {
    let (db, reg, person_type, order_type) = setup().await;
    let mut client = db.pool.get().await.unwrap();
    let raw: &mut forgebase_db::Client = &mut client;
    let mut tx = raw.transaction().await.unwrap();
    insert(
        &mut tx,
        &reg,
        &person_type.name,
        person("boss@x.y"),
        &Caller::system("cli"),
    )
    .await
    .unwrap();
    // A target inserted earlier in the same transaction can be linked.
    insert(
        &mut tx,
        &reg,
        &order_type.name,
        order(&[]),
        &Caller::system("cli"),
    )
    .await
    .unwrap();
    let error = delete(
        &mut tx,
        &reg,
        &person_type.name,
        "boss@x.y",
        1,
        &Caller::system("ann"),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(&error, DbError::LinkedDocumentExists { referenced_by: Some(t), .. } if t == "sales_order"),
        "{error}"
    );
    // The failed delete ran in a savepoint: the transaction is not aborted and still has its work.
    let boss = get(
        &mut tx,
        &reg,
        &person_type.name,
        "boss@x.y",
        &Caller::system("cli"),
    )
    .await
    .unwrap();
    assert!(boss.is_some());
    tx.rollback().await.unwrap();
    assert_eq!(count(&db, "doc_person").await, 0);
    assert_eq!(count(&db, "doc_sales_order").await, 0);
}

#[tokio::test]
async fn a_link_inside_a_rolled_back_transaction_leaves_nothing() {
    let (db, reg, person_type, order_type) = setup().await;
    let mut client = db.pool.get().await.unwrap();
    let raw: &mut forgebase_db::Client = &mut client;
    let mut tx = raw.transaction().await.unwrap();
    insert(
        &mut tx,
        &reg,
        &person_type.name,
        person("boss@x.y"),
        &Caller::system("cli"),
    )
    .await
    .unwrap();
    insert(
        &mut tx,
        &reg,
        &order_type.name,
        order(&[]),
        &Caller::system("cli"),
    )
    .await
    .unwrap();
    tx.rollback().await.unwrap();
    assert_eq!(count(&db, "doc_person").await, 0);
    assert_eq!(count(&db, "doc_sales_order").await, 0);
    // The target no longer exists, so linking to it fails.
    let error = insert(
        &mut **client,
        &reg,
        &order_type.name,
        order(&[]),
        &Caller::system("cli"),
    )
    .await
    .unwrap_err();
    assert!(matches!(error, DbError::Validation(_)), "{error}");
}

#[tokio::test]
async fn hostile_names_are_data_not_sql() {
    let (db, reg, person_type, order_type) = setup().await;
    let mut client = db.pool.get().await.unwrap();
    let nasty = "x'); DROP TABLE doc_person; --";
    insert(
        &mut **client,
        &reg,
        &person_type.name,
        person("boss@x.y"),
        &Caller::system("cli"),
    )
    .await
    .unwrap();
    let error = insert(
        &mut **client,
        &reg,
        &order_type.name,
        order(&[("customer", nasty)]),
        &Caller::system("cli"),
    )
    .await
    .unwrap_err();
    assert!(matches!(error, DbError::Validation(_)), "{error}");
    // The same string works as the name of a real target.
    insert(
        &mut **client,
        &reg,
        &person_type.name,
        person(nasty),
        &Caller::system("cli"),
    )
    .await
    .unwrap();
    let created = insert(
        &mut **client,
        &reg,
        &order_type.name,
        order(&[("customer", nasty)]),
        &Caller::system("cli"),
    )
    .await
    .unwrap();
    assert_eq!(created.values["customer"], text(nasty));
    assert_eq!(count(&db, "doc_person").await, 2);
}

#[tokio::test]
async fn link_target_with_random_naming_is_addressed_by_its_generated_name() {
    let db = TestDb::migrated().await;
    let registry = registry(&[
        "name = \"note\"\nlabel = \"N\"\n",
        "name = \"tag\"\nlabel = \"T\"\n[[fields]]\nname = \"note\"\ntype = \"link\"\ntarget = \"note\"\n",
    ]);
    db.sync(&registry).await.unwrap();
    let (note, tag) = (registry.get("note").unwrap(), registry.get("tag").unwrap());
    let mut client = db.pool.get().await.unwrap();
    let n = insert(
        &mut **client,
        &registry,
        &note.name,
        doc(&[]),
        &Caller::system("cli"),
    )
    .await
    .unwrap();
    let t = insert(
        &mut **client,
        &registry,
        &tag.name,
        doc(&[("note", text(&n.name))]),
        &Caller::system("cli"),
    )
    .await
    .unwrap();
    assert_eq!(t.values["note"], text(&n.name));
}

const BADGE: &str = r#"
name = "badge"
label = "Badge"
[[fields]]
name = "holder"
type = "link"
target = "person"
unique = true
"#;

#[tokio::test]
async fn a_duplicate_unique_link_names_the_field_not_the_internal_id() {
    let db = TestDb::migrated().await;
    let registry = registry(&[PERSON, BADGE]);
    db.sync(&registry).await.expect("sync");
    let (person_type, badge) = (
        registry.get("person").unwrap().clone(),
        registry.get("badge").unwrap().clone(),
    );
    let mut client = db.pool.get().await.unwrap();
    let ann = insert(
        &mut **client,
        &registry,
        &person_type.name,
        person("ann@x.y"),
        &Caller::system("cli"),
    )
    .await
    .unwrap();
    insert(
        &mut **client,
        &registry,
        &badge.name,
        doc(&[("holder", text(&ann.name))]),
        &Caller::system("cli"),
    )
    .await
    .unwrap();
    let error = insert(
        &mut **client,
        &registry,
        &badge.name,
        doc(&[("holder", text(&ann.name))]),
        &Caller::system("cli"),
    )
    .await
    .unwrap_err();
    let DbError::UniqueViolation { message } = &error else {
        panic!("expected unique violation, got {error}");
    };
    assert!(message.contains("`holder`"), "{message}");
    assert!(
        !message.contains("detail") && !message.contains("Key ("),
        "{message}"
    );
    let id = db.rows("SELECT id::text FROM doc_person").await[0][0].clone();
    assert!(!message.contains(&id), "{message}");
    assert_eq!(count(&db, "doc_badge").await, 1);
}

#[tokio::test]
async fn the_link_lookup_lock_blocks_deleting_the_target_until_the_transaction_ends() {
    let (db, reg, person_type, order_type) = setup().await;
    let boss = {
        let mut client = db.pool.get().await.unwrap();
        insert(
            &mut **client,
            &reg,
            &person_type.name,
            person("boss@x.y"),
            &Caller::system("cli"),
        )
        .await
        .unwrap()
    };
    let mut first = db.pool.get().await.unwrap();
    let mut second = db.pool.get().await.unwrap();
    second
        .batch_execute("SET lock_timeout = '300ms'")
        .await
        .unwrap();

    // The first transaction links to the boss: the lookup holds FOR KEY SHARE on the row.
    let raw: &mut forgebase_db::Client = &mut first;
    let mut tx = raw.transaction().await.unwrap();
    insert(
        &mut tx,
        &reg,
        &order_type.name,
        order(&[]),
        &Caller::system("cli"),
    )
    .await
    .unwrap();

    // A concurrent delete of the target waits and then times out.
    let error = delete(
        &mut **second,
        &reg,
        &person_type.name,
        &boss.name,
        1,
        &Caller::system("ann"),
    )
    .await
    .unwrap_err();
    assert!(matches!(error, DbError::LockTimeout), "{error}");
    assert_eq!(count(&db, "doc_person").await, 1);

    // After the rollback nothing links to the boss, so the delete succeeds.
    tx.rollback().await.unwrap();
    tokio::time::timeout(
        Duration::from_secs(5),
        delete(
            &mut **second,
            &reg,
            &person_type.name,
            &boss.name,
            1,
            &Caller::system("ann"),
        ),
    )
    .await
    .expect("delete should no longer wait")
    .unwrap();
    assert_eq!(count(&db, "doc_person").await, 0);
}
