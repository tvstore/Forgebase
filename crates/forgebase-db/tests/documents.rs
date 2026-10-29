//! Document CRUD against a real PostgreSQL (see `common`).

mod common;

use std::collections::BTreeMap;
use std::time::Duration;

use common::{CUSTOMER, TestDb, registry};
use forgebase_core::document::{Changes, Document, ValidationError};
use forgebase_core::meta::{DocType, Registry};
use forgebase_core::value::Value;
use forgebase_db::{DbError, delete, get, insert, update};
use macros_free::{date, datetime};
use rust_decimal::Decimal;

/// Small constructors for `time` values without its `macros` feature.
mod macros_free {
    use time::{Date, Month, OffsetDateTime, UtcOffset};

    pub fn date(y: i32, m: u8, d: u8) -> Date {
        Date::from_calendar_date(y, Month::try_from(m).expect("month"), d).expect("date")
    }

    /// 2024-03-15 10:20:30.123456 at the given offset hours.
    pub fn datetime(offset_hours: i8) -> OffsetDateTime {
        let utc = OffsetDateTime::from_unix_timestamp_nanos(1_710_498_030_123_456_000)
            .expect("timestamp");
        utc.to_offset(UtcOffset::from_hms(offset_hours, 0, 0).expect("offset"))
    }
}

/// A DocType whose name is the value of `email`.
const NAMED: &str = r#"
name = "account"
label = "Account"
naming = { field = "email" }
[[fields]]
name = "email"
type = "data"
required = true
unique = true
[[fields]]
name = "points"
type = "int"
"#;

async fn setup(texts: &[&str]) -> (TestDb, Registry, Vec<DocType>) {
    let db = TestDb::migrated().await;
    let registry = registry(texts);
    db.sync(&registry).await.expect("sync");
    let doctypes = registry.iter().cloned().collect();
    (db, registry, doctypes)
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

/// Changes to field values only.
fn values_of<const N: usize>(pairs: [(String, Value); N]) -> Changes {
    Changes {
        values: BTreeMap::from(pairs),
        tables: BTreeMap::new(),
    }
}

fn text(s: &str) -> Value {
    Value::Text(s.to_owned())
}

fn dec(s: &str) -> Value {
    Value::Decimal(s.parse::<Decimal>().expect("decimal"))
}

fn full_customer(email: &str) -> Document {
    doc(&[
        ("email", text(email)),
        ("notes", text("line one\nline two \u{1F600}")),
        ("points", Value::Int(-42)),
        ("credit_limit", dec("1234.50")),
        ("active", Value::Bool(true)),
        ("joined_on", Value::Date(date(2024, 3, 15))),
        ("last_seen", Value::Datetime(datetime(0))),
        ("tier", text("gold")),
    ])
}

async fn count(db: &TestDb, table: &str) -> i64 {
    db.rows(&format!("SELECT count(*)::text FROM {table}"))
        .await[0][0]
        .parse()
        .expect("count")
}

#[tokio::test]
async fn round_trips_every_field_type() {
    let (db, reg, types) = setup(&[CUSTOMER]).await;
    let customer = &types[0];
    let mut client = db.pool.get().await.unwrap();

    let stored = insert(
        &mut **client,
        &reg,
        &customer.name,
        full_customer("a@example.com"),
        "alice",
    )
    .await
    .unwrap();
    assert_eq!(stored.values["email"], text("a@example.com"));
    assert_eq!(stored.values["points"], Value::Int(-42));
    assert_eq!(stored.values["credit_limit"], dec("1234.50"));
    assert_eq!(stored.values["active"], Value::Bool(true));
    assert_eq!(stored.values["joined_on"], Value::Date(date(2024, 3, 15)));
    assert_eq!(stored.values["last_seen"], Value::Datetime(datetime(0)));
    assert_eq!(stored.values["tier"], text("gold"));
    assert_eq!(stored.values.len(), 8);
    assert_eq!(stored.owner, "alice");
    assert_eq!(stored.modified_by, "alice");
    assert_eq!(stored.version, 1);
    assert_eq!(stored.created_at, stored.modified_at);

    let fetched = get(&mut **client, &reg, &customer.name, &stored.name)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(fetched, stored);
}

#[tokio::test]
async fn absent_and_null_values_are_null() {
    let (db, reg, types) = setup(&[CUSTOMER]).await;
    let mut client = db.pool.get().await.unwrap();
    let stored = insert(
        &mut **client,
        &reg,
        &types[0].name,
        doc(&[("email", text("n@example.com")), ("points", Value::Null)]),
        "cli",
    )
    .await
    .unwrap();
    assert_eq!(stored.values.len(), 8);
    for (field, value) in &stored.values {
        if field != "email" {
            assert_eq!(value, &Value::Null, "{field}");
        }
    }
}

#[tokio::test]
async fn boundary_values_round_trip() {
    let (db, reg, types) = setup(&[CUSTOMER]).await;
    let customer = &types[0];
    let mut client = db.pool.get().await.unwrap();

    // Data at max_length 100, counted in characters (multi-byte characters).
    let longest = "\u{e9}".repeat(100);
    // Decimal(12,2): the largest and smallest values and the smallest step.
    for (i, (email, decimal)) in [
        (longest.as_str(), "9999999999.99"),
        ("neg", "-9999999999.99"),
        ("tiny", "0.01"),
        ("zero", "0"),
    ]
    .into_iter()
    .enumerate()
    {
        let stored = insert(
            &mut **client,
            &reg,
            &customer.name,
            doc(&[("email", text(email)), ("credit_limit", dec(decimal))]),
            "cli",
        )
        .await
        .unwrap();
        assert_eq!(stored.values["email"], text(email), "{i}");
        assert_eq!(stored.values["credit_limit"], dec(decimal), "{i}");
    }
    // i64 extremes.
    for n in [i64::MIN, i64::MAX, 0] {
        let stored = insert(
            &mut **client,
            &reg,
            &customer.name,
            doc(&[
                ("email", text(&format!("int{n}"))),
                ("points", Value::Int(n)),
            ]),
            "cli",
        )
        .await
        .unwrap();
        assert_eq!(stored.values["points"], Value::Int(n));
    }
    // A non-UTC offset is stored as the same instant (and comes back in UTC).
    let local = datetime(7);
    let stored = insert(
        &mut **client,
        &reg,
        &customer.name,
        doc(&[("email", text("tz")), ("last_seen", Value::Datetime(local))]),
        "cli",
    )
    .await
    .unwrap();
    let Value::Datetime(back) = &stored.values["last_seen"] else {
        panic!("not a datetime");
    };
    assert_eq!(*back, local);
    assert_eq!(back.unix_timestamp_nanos(), local.unix_timestamp_nanos());
}

#[tokio::test]
async fn random_naming_uses_the_id() {
    let (db, reg, types) = setup(&[CUSTOMER]).await;
    let mut client = db.pool.get().await.unwrap();
    let stored = insert(
        &mut **client,
        &reg,
        &types[0].name,
        full_customer("r@example.com"),
        "cli",
    )
    .await
    .unwrap();
    assert_eq!(stored.name, stored.id.hyphenated().to_string());
    assert_eq!(stored.id.get_version_num(), 7);
    let again = get(&mut **client, &reg, &types[0].name, &stored.name)
        .await
        .unwrap();
    assert_eq!(again.unwrap().id, stored.id);
}

#[tokio::test]
async fn explicit_name_is_rejected_for_random_naming() {
    let (db, reg, types) = setup(&[CUSTOMER]).await;
    let mut client = db.pool.get().await.unwrap();
    let mut document = full_customer("x@example.com");
    document.name = Some("my-name".to_owned());
    let error = insert(&mut **client, &reg, &types[0].name, document, "cli")
        .await
        .unwrap_err();
    assert!(matches!(error, DbError::Naming(_)), "{error}");
    assert_eq!(count(&db, "doc_customer").await, 0);
}

#[tokio::test]
async fn field_naming_uses_the_field_value() {
    let (db, reg, types) = setup(&[NAMED]).await;
    let account = &types[0];
    let mut client = db.pool.get().await.unwrap();

    let stored = insert(
        &mut **client,
        &reg,
        &account.name,
        doc(&[("email", text("a@b.c"))]),
        "cli",
    )
    .await
    .unwrap();
    assert_eq!(stored.name, "a@b.c");

    // An explicit name that equals the field value is accepted, a different one is not.
    let mut same = doc(&[("email", text("same"))]);
    same.name = Some("same".to_owned());
    assert_eq!(
        insert(&mut **client, &reg, &account.name, same, "cli")
            .await
            .unwrap()
            .name,
        "same"
    );
    let mut different = doc(&[("email", text("other"))]);
    different.name = Some("nope".to_owned());
    let error = insert(&mut **client, &reg, &account.name, different, "cli")
        .await
        .unwrap_err();
    assert!(matches!(error, DbError::Naming(_)), "{error}");
    assert!(
        get(&mut **client, &reg, &account.name, "other")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn validation_errors_are_collected_and_nothing_is_written() {
    let (db, reg, types) = setup(&[CUSTOMER]).await;
    let mut client = db.pool.get().await.unwrap();
    let error = insert(
        &mut **client,
        &reg,
        &types[0].name,
        doc(&[
            ("email", text("   ")),
            ("bogus", Value::Int(1)),
            ("tier", text("platinum")),
            ("points", text("many")),
        ]),
        "cli",
    )
    .await
    .unwrap_err();
    let DbError::Validation(errors) = &error else {
        panic!("expected validation error, got {error}");
    };
    assert_eq!(errors.len(), 4, "{errors:?}");
    assert!(errors.contains(&ValidationError::Required {
        field: "email".to_owned()
    }));
    assert!(errors.contains(&ValidationError::UnknownField {
        field: "bogus".to_owned()
    }));
    let shown = error.to_string();
    assert!(
        shown.contains("bogus") && shown.contains("platinum"),
        "{shown}"
    );
    assert_eq!(count(&db, "doc_customer").await, 0);

    // A required field that is missing entirely.
    let error = insert(&mut **client, &reg, &types[0].name, doc(&[]), "cli")
        .await
        .unwrap_err();
    assert!(
        matches!(error, DbError::Validation(ref e) if e.len() == 1),
        "{error}"
    );
}

#[tokio::test]
async fn invalid_actor_is_rejected() {
    let (db, reg, types) = setup(&[CUSTOMER]).await;
    let mut client = db.pool.get().await.unwrap();
    for actor in ["", "a\0b"] {
        let error = insert(
            &mut **client,
            &reg,
            &types[0].name,
            full_customer("a@x.y"),
            actor,
        )
        .await
        .unwrap_err();
        assert!(matches!(error, DbError::InvalidActor), "{error}");
        let changes = values_of([("points".to_owned(), Value::Int(1))]);
        let error = update(&mut **client, &reg, &types[0].name, "n", 1, &changes, actor)
            .await
            .unwrap_err();
        assert!(matches!(error, DbError::InvalidActor), "{error}");
    }
    assert_eq!(count(&db, "doc_customer").await, 0);
}

#[tokio::test]
async fn get_missing_is_none() {
    let (db, reg, types) = setup(&[CUSTOMER]).await;
    let mut client = db.pool.get().await.unwrap();
    assert!(
        get(&mut **client, &reg, &types[0].name, "nothing")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn update_changes_values_and_bumps_version() {
    let (db, reg, types) = setup(&[CUSTOMER]).await;
    let customer = &types[0];
    let mut client = db.pool.get().await.unwrap();
    let created = insert(
        &mut **client,
        &reg,
        &customer.name,
        full_customer("u@example.com"),
        "alice",
    )
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;

    let changes = values_of([
        ("points".to_owned(), Value::Int(7)),
        ("credit_limit".to_owned(), dec("0.5")),
        ("notes".to_owned(), Value::Null),
    ]);
    let updated = update(
        &mut **client,
        &reg,
        &customer.name,
        &created.name,
        1,
        &changes,
        "bob",
    )
    .await
    .unwrap();
    assert_eq!(updated.version, 2);
    assert_eq!(updated.modified_by, "bob");
    assert!(updated.modified_at > created.modified_at);
    assert_eq!(updated.created_at, created.created_at);
    assert_eq!(updated.owner, "alice");
    assert_eq!(updated.id, created.id);
    assert_eq!(updated.values["points"], Value::Int(7));
    assert_eq!(updated.values["credit_limit"], dec("0.50"));
    assert_eq!(updated.values["notes"], Value::Null);
    // Untouched fields stay.
    assert_eq!(updated.values["tier"], text("gold"));
    assert_eq!(updated.values["email"], created.values["email"]);

    let fetched = get(&mut **client, &reg, &customer.name, &created.name)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(fetched, updated);
    let third = update(
        &mut **client,
        &reg,
        &customer.name,
        &created.name,
        2,
        &changes,
        "bob",
    )
    .await
    .unwrap();
    assert_eq!(third.version, 3);
}

#[tokio::test]
async fn update_rejects_bad_changes() {
    let (db, reg, types) = setup(&[CUSTOMER, NAMED]).await;
    let (account, customer) = (&types[0], &types[1]);
    assert_eq!(account.name, "account");
    let mut client = db.pool.get().await.unwrap();
    let created = insert(
        &mut **client,
        &reg,
        &customer.name,
        full_customer("v@example.com"),
        "cli",
    )
    .await
    .unwrap();

    // Nothing to change.
    let error = update(
        &mut **client,
        &reg,
        &customer.name,
        &created.name,
        1,
        &Changes::default(),
        "cli",
    )
    .await
    .unwrap_err();
    assert!(matches!(error, DbError::NoChanges), "{error}");
    // Validation (unknown field, required cleared).
    let changes = values_of([
        ("bogus".to_owned(), Value::Int(1)),
        ("email".to_owned(), Value::Null),
    ]);
    let error = update(
        &mut **client,
        &reg,
        &customer.name,
        &created.name,
        1,
        &changes,
        "cli",
    )
    .await
    .unwrap_err();
    assert!(
        matches!(error, DbError::Validation(ref e) if e.len() == 2),
        "{error}"
    );

    // The naming field cannot change, even to the same value.
    let named = insert(
        &mut **client,
        &reg,
        &account.name,
        doc(&[("email", text("old"))]),
        "cli",
    )
    .await
    .unwrap();
    for new in ["new", "old"] {
        let changes = values_of([("email".to_owned(), text(new))]);
        let error = update(
            &mut **client,
            &reg,
            &account.name,
            "old",
            1,
            &changes,
            "cli",
        )
        .await
        .unwrap_err();
        assert!(matches!(error, DbError::Naming(_)), "{error}");
    }
    assert_eq!(
        get(&mut **client, &reg, &account.name, "old")
            .await
            .unwrap()
            .unwrap(),
        named
    );

    // Unknown name.
    let changes = values_of([("points".to_owned(), Value::Int(1))]);
    let error = update(
        &mut **client,
        &reg,
        &customer.name,
        "missing",
        1,
        &changes,
        "cli",
    )
    .await
    .unwrap_err();
    assert!(
        matches!(&error, DbError::NotFound { doctype, name } if doctype == "customer" && name == "missing"),
        "{error}"
    );
    // Nothing changed.
    let unchanged = get(&mut **client, &reg, &customer.name, &created.name)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(unchanged, created);
}

#[tokio::test]
async fn delete_removes_the_document() {
    let (db, reg, types) = setup(&[CUSTOMER]).await;
    let customer = &types[0];
    let mut client = db.pool.get().await.unwrap();
    let stored = insert(
        &mut **client,
        &reg,
        &customer.name,
        full_customer("d@example.com"),
        "cli",
    )
    .await
    .unwrap();
    delete(&mut **client, &reg, &customer.name, &stored.name, 1)
        .await
        .unwrap();
    assert!(
        get(&mut **client, &reg, &customer.name, &stored.name)
            .await
            .unwrap()
            .is_none()
    );
    let error = delete(&mut **client, &reg, &customer.name, &stored.name, 1)
        .await
        .unwrap_err();
    assert!(matches!(error, DbError::NotFound { .. }), "{error}");
}

#[tokio::test]
async fn duplicate_unique_values_are_unique_violations() {
    let (db, reg, types) = setup(&[CUSTOMER, NAMED]).await;
    let (account, customer) = (&types[0], &types[1]);
    let mut client = db.pool.get().await.unwrap();

    insert(
        &mut **client,
        &reg,
        &customer.name,
        full_customer("dup@example.com"),
        "cli",
    )
    .await
    .unwrap();
    let error = insert(
        &mut **client,
        &reg,
        &customer.name,
        full_customer("dup@example.com"),
        "cli",
    )
    .await
    .unwrap_err();
    let DbError::UniqueViolation { message } = &error else {
        panic!("expected unique violation, got {error}");
    };
    assert!(
        message.contains("unique") && message.contains("23505"),
        "{message}"
    );
    assert_eq!(count(&db, "doc_customer").await, 1);

    // Update into a duplicate.
    let other = insert(
        &mut **client,
        &reg,
        &customer.name,
        full_customer("other@example.com"),
        "cli",
    )
    .await
    .unwrap();
    let changes = values_of([("email".to_owned(), text("dup@example.com"))]);
    let error = update(
        &mut **client,
        &reg,
        &customer.name,
        &other.name,
        1,
        &changes,
        "cli",
    )
    .await
    .unwrap_err();
    assert!(matches!(error, DbError::UniqueViolation { .. }), "{error}");

    // Field naming: the same name twice.
    insert(
        &mut **client,
        &reg,
        &account.name,
        doc(&[("email", text("n"))]),
        "cli",
    )
    .await
    .unwrap();
    let error = insert(
        &mut **client,
        &reg,
        &account.name,
        doc(&[("email", text("n"))]),
        "cli",
    )
    .await
    .unwrap_err();
    assert!(matches!(error, DbError::UniqueViolation { .. }), "{error}");
}

#[tokio::test]
async fn injection_like_strings_are_stored_literally() {
    let (db, reg, types) = setup(&[CUSTOMER]).await;
    let customer = &types[0];
    let mut client = db.pool.get().await.unwrap();
    let nasty = [
        "x'); DROP TABLE doc_customer; --",
        "\"; DROP TABLE doc_customer; --",
        "$1 $2 ${x} \\' %s",
    ];
    for (i, value) in nasty.iter().enumerate() {
        let stored = insert(
            &mut **client,
            &reg,
            &customer.name,
            doc(&[("email", text(&format!("{i}@x.y"))), ("notes", text(value))]),
            value,
        )
        .await
        .unwrap();
        assert_eq!(stored.values["notes"], text(value));
        assert_eq!(stored.owner, *value);
        let changes = values_of([("notes".to_owned(), text(value))]);
        let updated = update(
            &mut **client,
            &reg,
            &customer.name,
            &stored.name,
            1,
            &changes,
            value,
        )
        .await
        .unwrap();
        assert_eq!(updated.modified_by, *value);
        // Looking up by a hostile name finds nothing and breaks nothing.
        assert!(
            get(&mut **client, &reg, &customer.name, value)
                .await
                .unwrap()
                .is_none()
        );
        assert!(matches!(
            delete(&mut **client, &reg, &customer.name, value, 1).await,
            Err(DbError::NotFound { .. })
        ));
    }
    assert_eq!(count(&db, "doc_customer").await, 3);
}

#[tokio::test]
async fn operations_in_a_rolled_back_transaction_leave_nothing() {
    let (db, reg, types) = setup(&[CUSTOMER]).await;
    let customer = &types[0];
    let mut client = db.pool.get().await.unwrap();
    let raw: &mut forgebase_db::Client = &mut client;

    let mut tx = raw.transaction().await.unwrap();
    let stored = insert(
        &mut tx,
        &reg,
        &customer.name,
        full_customer("t@example.com"),
        "cli",
    )
    .await
    .unwrap();
    let changes = values_of([("points".to_owned(), Value::Int(9))]);
    update(
        &mut tx,
        &reg,
        &customer.name,
        &stored.name,
        1,
        &changes,
        "cli",
    )
    .await
    .unwrap();
    // Visible inside the transaction.
    assert!(
        get(&mut tx, &reg, &customer.name, &stored.name)
            .await
            .unwrap()
            .is_some()
    );
    tx.rollback().await.unwrap();
    assert_eq!(count(&db, "doc_customer").await, 0);

    // A committed transaction keeps the document.
    let mut tx = raw.transaction().await.unwrap();
    insert(
        &mut tx,
        &reg,
        &customer.name,
        full_customer("t@example.com"),
        "cli",
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(count(&db, "doc_customer").await, 1);
}

#[tokio::test]
async fn stored_document_columns_match_the_table() {
    // Documents the standard-column order of the generated DDL. Decoding does not depend on it
    // (every statement selects an explicit column list); this only pins the table layout.
    let (db, _, _) = setup(&[CUSTOMER]).await;
    let names: Vec<String> = db
        .columns("doc_customer")
        .await
        .into_iter()
        .map(|(name, _)| name)
        .take(7)
        .collect();
    assert_eq!(
        names,
        [
            "id",
            "name",
            "owner",
            "created_at",
            "modified_at",
            "modified_by",
            "version"
        ]
    );
}

#[tokio::test]
async fn datetime_out_of_range_in_utc_is_an_error_not_a_panic() {
    let (db, reg, types) = setup(&[CUSTOMER]).await;
    let mut client = db.pool.get().await.unwrap();
    // 9999-12-31T23:00:00-05:00 is year 10000 in UTC.
    let edge = time::Date::from_calendar_date(9999, time::Month::December, 31)
        .unwrap()
        .with_hms(23, 0, 0)
        .unwrap()
        .assume_offset(time::UtcOffset::from_hms(-5, 0, 0).unwrap());
    let error = insert(
        &mut **client,
        &reg,
        &types[0].name,
        doc(&[
            ("email", text("e@x.y")),
            ("last_seen", Value::Datetime(edge)),
        ]),
        "cli",
    )
    .await
    .unwrap_err();
    assert!(matches!(error, DbError::Postgres(_)), "{error}");
    assert_eq!(count(&db, "doc_customer").await, 0);
}

#[tokio::test]
async fn unreadable_stored_values_give_an_error_naming_the_field() {
    let (db, reg, types) = setup(&[CUSTOMER]).await;
    let mut client = db.pool.get().await.unwrap();
    let stored = insert(
        &mut **client,
        &reg,
        &types[0].name,
        full_customer("raw@example.com"),
        "cli",
    )
    .await
    .unwrap();
    // Another writer stores a date the `time` crate cannot represent.
    db.exec("UPDATE doc_customer SET joined_on = date '5874897-12-31'")
        .await;
    let error = get(&mut **client, &reg, &types[0].name, &stored.name)
        .await
        .unwrap_err();
    assert!(matches!(error, DbError::Decode { .. }), "{error:?}");
    let message = error.to_string();
    assert!(message.contains("`joined_on`"), "{message}");
    assert!(message.contains("`customer`"), "{message}");

    // Same for timestamptz infinity.
    db.exec("UPDATE doc_customer SET joined_on = NULL, last_seen = 'infinity'")
        .await;
    let message = get(&mut **client, &reg, &types[0].name, &stored.name)
        .await
        .unwrap_err()
        .to_string();
    assert!(message.contains("`last_seen`"), "{message}");
}
