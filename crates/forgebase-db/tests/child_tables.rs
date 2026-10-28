//! Child tables (Table fields): schema sync and document CRUD against a real PostgreSQL (see
//! `common`).

mod common;

use std::collections::BTreeMap;
use std::time::Duration;

use common::{PERSON, TestDb, registry};
use forgebase_core::document::{Changes, Document, Row, ValidationError};
use forgebase_core::meta::Registry;
use forgebase_core::value::Value;
use forgebase_db::{Conflict, DbError, Step, SyncOptions, delete, get, insert, sync, update};
use rust_decimal::Decimal;

const INVOICE_ITEM: &str = r#"
name = "invoice_item"
label = "Invoice Item"
is_child = true
[[fields]]
name = "qty"
type = "int"
required = true
[[fields]]
name = "rate"
type = "decimal"
precision = 10
scale = 2
[[fields]]
name = "description"
type = "data"
max_length = 50
[[fields]]
name = "supplier"
type = "link"
target = "person"
"#;

fn invoice_toml(required: bool) -> String {
    format!(
        "name = \"invoice\"\nlabel = \"Invoice\"\n\
         [[fields]]\nname = \"title\"\ntype = \"data\"\n\
         [[fields]]\nname = \"items\"\ntype = \"table\"\ntarget = \"invoice_item\"\nrequired = {required}\n"
    )
}

fn invoice_registry(required: bool) -> Registry {
    registry(&[PERSON, &invoice_toml(required), INVOICE_ITEM])
}

async fn setup() -> (TestDb, Registry) {
    let db = TestDb::migrated().await;
    let reg = invoice_registry(false);
    db.sync(&reg).await.expect("sync");
    (db, reg)
}

fn text(s: &str) -> Value {
    Value::Text(s.to_owned())
}

fn item(qty: i64, description: &str) -> Row {
    BTreeMap::from([
        ("qty".to_owned(), Value::Int(qty)),
        ("description".to_owned(), text(description)),
    ])
}

fn invoice(title: &str, rows: Vec<Row>) -> Document {
    Document {
        name: None,
        values: BTreeMap::from([("title".to_owned(), text(title))]),
        tables: BTreeMap::from([("items".to_owned(), rows)]),
    }
}

fn person(email: &str) -> Document {
    Document {
        name: None,
        values: BTreeMap::from([("email".to_owned(), text(email))]),
        tables: BTreeMap::new(),
    }
}

fn rows_change(rows: Vec<Row>) -> Changes {
    Changes {
        values: BTreeMap::new(),
        tables: BTreeMap::from([("items".to_owned(), rows)]),
    }
}

fn descriptions(rows: &[Row]) -> Vec<String> {
    rows.iter()
        .map(|r| match &r["description"] {
            Value::Text(s) => s.clone(),
            other => panic!("{other:?}"),
        })
        .collect()
}

async fn count(db: &TestDb, table: &str) -> i64 {
    db.rows(&format!("SELECT count(*)::text FROM {table}"))
        .await[0][0]
        .parse()
        .expect("count")
}

/// The foreign keys of a table as `pg_get_constraintdef` text, sorted.
async fn foreign_keys(db: &TestDb, table: &str) -> Vec<String> {
    db.rows(&format!(
        "SELECT pg_get_constraintdef(oid) FROM pg_constraint \
         WHERE conrelid = '{table}'::regclass AND contype = 'f' ORDER BY 1"
    ))
    .await
    .into_iter()
    .map(|mut r| r.remove(0))
    .collect()
}

/// Single-column indexes of a table, as `column (unique|plain)`.
async fn indexed_columns(db: &TestDb, table: &str) -> Vec<String> {
    db.rows(&format!(
        "SELECT a.attname::text || CASE WHEN i.indisunique THEN ' (unique)' ELSE ' (plain)' END \
         FROM pg_index i JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = i.indkey[0] \
         WHERE i.indrelid = '{table}'::regclass AND i.indnatts = 1 ORDER BY 1"
    ))
    .await
    .into_iter()
    .map(|mut r| r.remove(0))
    .collect()
}

fn conflict_texts(error: DbError) -> Vec<String> {
    match error {
        DbError::SyncConflicts(conflicts) => conflicts.iter().map(ToString::to_string).collect(),
        other => panic!("expected conflicts, got {other}"),
    }
}

// ---------------------------------------------------------------------------------------------
// Schema sync
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn parent_and_child_tables_are_created_in_one_sync() {
    let db = TestDb::migrated().await;
    let reg = invoice_registry(false);
    let report = db.sync(&reg).await.unwrap();
    assert!(
        report
            .steps
            .iter()
            .any(|s| matches!(s, Step::CreateTable { doctype, .. } if doctype == "invoice_item"))
    );

    // The child has the child standard columns and no name, owner, version or timestamps.
    assert_eq!(
        db.columns("doc_invoice_item").await,
        [
            ("id", "uuid/NO"),
            ("parent_id", "uuid/NO"),
            ("idx", "integer/NO"),
            ("qty", "bigint/YES"),
            ("rate", "numeric(10,2)/YES"),
            ("description", "character varying(50)/YES"),
            ("supplier", "uuid/YES"),
        ]
        .map(|(c, t)| (c.to_owned(), t.to_owned()))
    );
    // The Table field is not a column of the parent.
    let parent_columns: Vec<String> = db
        .columns("doc_invoice")
        .await
        .into_iter()
        .map(|(c, _)| c)
        .collect();
    assert!(parent_columns.contains(&"title".to_owned()));
    assert!(
        !parent_columns.contains(&"items".to_owned()),
        "{parent_columns:?}"
    );

    assert_eq!(
        foreign_keys(&db, "doc_invoice_item").await,
        [
            "FOREIGN KEY (parent_id) REFERENCES doc_invoice(id) ON DELETE CASCADE",
            "FOREIGN KEY (supplier) REFERENCES doc_person(id) ON DELETE RESTRICT",
        ]
    );
    assert_eq!(
        indexed_columns(&db, "doc_invoice_item").await,
        ["id (unique)", "parent_id (plain)", "supplier (plain)"]
    );
    assert_eq!(
        db.rows("SELECT name::text FROM fb_doctype ORDER BY 1")
            .await,
        [["invoice"], ["invoice_item"], ["person"]]
    );

    // A second sync has nothing to do.
    let again = db.sync(&reg).await.unwrap();
    assert!(again.steps.is_empty(), "{again:?}");
    assert!(again.warnings.is_empty(), "{again:?}");
}

#[tokio::test]
async fn a_table_field_and_its_child_can_be_added_later() {
    let db = TestDb::migrated().await;
    let plain =
        "name = \"invoice\"\nlabel = \"Invoice\"\n[[fields]]\nname = \"title\"\ntype = \"data\"\n";
    db.sync(&registry(&[PERSON, plain])).await.unwrap();
    let report = db.sync(&invoice_registry(false)).await.unwrap();
    let kinds: Vec<&str> = report
        .steps
        .iter()
        .map(|s| match s {
            Step::CreateTable { .. } => "table",
            Step::AddKeys { .. } => "keys",
            Step::AddForeignKeys { .. } => "fks",
            Step::CreateIndex { .. } => "index",
            Step::RegisterDocType { .. } => "register",
            other => panic!("{other}"),
        })
        .collect();
    // The parent table is untouched: no column for the Table field.
    assert_eq!(
        kinds,
        ["table", "keys", "fks", "index", "index", "register"]
    );
    assert_eq!(
        foreign_keys(&db, "doc_invoice_item").await[0],
        "FOREIGN KEY (parent_id) REFERENCES doc_invoice(id) ON DELETE CASCADE"
    );
}

#[tokio::test]
async fn a_dropped_or_changed_parent_key_is_a_conflict() {
    let db = TestDb::migrated().await;
    let reg = invoice_registry(false);
    db.sync(&reg).await.unwrap();
    let constraint = db
        .rows(
            "SELECT conname::text FROM pg_constraint WHERE conrelid = 'doc_invoice_item'::regclass \
             AND contype = 'f' AND pg_get_constraintdef(oid) LIKE 'FOREIGN KEY (parent_id)%'",
        )
        .await[0][0]
        .clone();

    // ON DELETE changed from CASCADE to RESTRICT.
    db.exec(&format!(
        "ALTER TABLE doc_invoice_item DROP CONSTRAINT {constraint}; \
         ALTER TABLE doc_invoice_item ADD FOREIGN KEY (parent_id) REFERENCES doc_invoice (id) ON DELETE RESTRICT"
    ))
    .await;
    let texts = conflict_texts(db.sync(&reg).await.unwrap_err());
    assert_eq!(texts.len(), 1, "{texts:?}");
    assert!(
        texts[0].contains("doc_invoice_item.parent_id")
            && texts[0].contains("ON DELETE RESTRICT")
            && texts[0].contains("ON DELETE CASCADE"),
        "{texts:?}"
    );

    // The key is gone.
    db.exec("ALTER TABLE doc_invoice_item DROP CONSTRAINT doc_invoice_item_parent_id_fkey")
        .await;
    let texts = conflict_texts(db.sync(&reg).await.unwrap_err());
    assert_eq!(texts.len(), 1, "{texts:?}");
    assert!(texts[0].contains("is missing in the database"), "{texts:?}");

    // Pointing at the wrong table.
    db.exec(
        "ALTER TABLE doc_invoice_item ADD FOREIGN KEY (parent_id) REFERENCES doc_person (id) ON DELETE CASCADE",
    )
    .await;
    let texts = conflict_texts(db.sync(&reg).await.unwrap_err());
    assert_eq!(texts.len(), 1, "{texts:?}");
    assert!(texts[0].contains("doc_person"), "{texts:?}");
}

#[tokio::test]
async fn a_missing_parent_index_is_added() {
    let db = TestDb::migrated().await;
    let reg = invoice_registry(false);
    db.sync(&reg).await.unwrap();
    db.exec("DROP INDEX doc_invoice_item_parent_id_idx").await;
    let report = db.sync(&reg).await.unwrap();
    assert!(
        matches!(&report.steps[..], [Step::CreateIndex { column, .. }] if column == "parent_id"),
        "{:?}",
        report.steps
    );
    assert_eq!(
        indexed_columns(&db, "doc_invoice_item").await,
        ["id (unique)", "parent_id (plain)", "supplier (plain)"]
    );
}

#[tokio::test]
async fn a_missing_child_standard_column_is_a_conflict() {
    let db = TestDb::migrated().await;
    let reg = invoice_registry(false);
    db.sync(&reg).await.unwrap();
    db.exec("ALTER TABLE doc_invoice_item ALTER COLUMN idx TYPE bigint")
        .await;
    let texts = conflict_texts(db.sync(&reg).await.unwrap_err());
    assert_eq!(texts.len(), 1, "{texts:?}");
    assert!(
        texts[0].contains("doc_invoice_item.idx is bigint NOT NULL")
            && texts[0].contains("requires integer NOT NULL"),
        "{texts:?}"
    );
}

#[tokio::test]
async fn a_leftover_name_column_of_a_child_is_only_a_warning() {
    let db = TestDb::migrated().await;
    let reg = invoice_registry(false);
    db.sync(&reg).await.unwrap();
    db.exec("ALTER TABLE doc_invoice_item ADD COLUMN name text")
        .await;
    let report = db.sync(&reg).await.unwrap();
    assert!(report.steps.is_empty());
    assert_eq!(
        report
            .warnings
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        ["column doc_invoice_item.name is not in the metadata; kept"]
    );
}

#[tokio::test]
async fn switching_a_doctype_between_child_and_not_child_is_a_conflict() {
    let db = TestDb::migrated().await;
    // `invoice_item` first exists as a normal DocType ...
    let plain_item = INVOICE_ITEM.replace("is_child = true\n", "");
    let plain_invoice =
        "name = \"invoice\"\nlabel = \"Invoice\"\n[[fields]]\nname = \"title\"\ntype = \"data\"\n";
    db.sync(&registry(&[PERSON, plain_invoice, &plain_item]))
        .await
        .unwrap();
    // ... and is then declared a child: refused, nothing applied.
    let texts = conflict_texts(db.sync(&invoice_registry(false)).await.unwrap_err());
    assert_eq!(texts.len(), 2, "{texts:?}");
    assert!(
        texts[0].contains("doc_invoice_item.parent_id is missing"),
        "{texts:?}"
    );
    assert!(
        texts[1].contains("doc_invoice_item.idx is missing"),
        "{texts:?}"
    );
    assert_eq!(
        db.columns("doc_invoice_item").await.len(),
        7 + 4,
        "the table is unchanged"
    );
    let error = db.sync(&invoice_registry(false)).await.unwrap_err();
    assert!(matches!(
        error,
        DbError::SyncConflicts(c) if matches!(c[0], Conflict::ColumnMismatch { .. })
    ));

    // The other way round: a child table that is declared a normal DocType.
    let db = TestDb::migrated().await;
    db.sync(&invoice_registry(false)).await.unwrap();
    let texts = conflict_texts(
        db.sync(&registry(&[PERSON, plain_invoice, &plain_item]))
            .await
            .unwrap_err(),
    );
    assert!(
        texts
            .iter()
            .any(|t| t.contains("doc_invoice_item.name is missing")),
        "{texts:?}"
    );
}

#[tokio::test]
async fn an_unused_child_is_refused_before_the_database_is_touched() {
    let db = TestDb::migrated().await;
    let mut client = db.pool.get().await.unwrap();
    let error = sync(
        &mut client,
        &registry(&[PERSON, INVOICE_ITEM]),
        &SyncOptions::default(),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(&error, DbError::InvalidMetadata(e) if e.len() == 1),
        "{error}"
    );
    assert_eq!(
        error.to_string(),
        "invalid DocType metadata: child doctype `invoice_item` is not used by any table field"
    );
    drop(client);
    assert_eq!(
        db.rows("SELECT count(*)::text FROM fb_doctype").await[0][0],
        "0"
    );
}

#[tokio::test]
async fn dry_run_shows_the_sql_of_a_parent_and_its_child() {
    let db = TestDb::migrated().await;
    let mut client = db.pool.get().await.unwrap();
    let options = SyncOptions {
        dry_run: true,
        ..SyncOptions::default()
    };
    let report = sync(&mut client, &invoice_registry(false), &options)
        .await
        .unwrap();
    let text: Vec<String> = report.steps.iter().map(ToString::to_string).collect();
    assert!(
        text.contains(
            &"CREATE TABLE \"doc_invoice_item\" (\n    \"id\" uuid NOT NULL,\n    \"parent_id\" uuid NOT NULL,\n    \
              \"idx\" integer NOT NULL,\n    \"qty\" bigint,\n    \"rate\" numeric(10,2),\n    \
              \"description\" character varying(50),\n    \"supplier\" uuid\n);"
                .to_owned()
        ),
        "{text:#?}"
    );
    assert!(
        text.contains(
            &"ALTER TABLE \"doc_invoice_item\" ADD FOREIGN KEY (\"parent_id\") REFERENCES \"doc_invoice\" (\"id\") ON DELETE CASCADE, \
              ADD FOREIGN KEY (\"supplier\") REFERENCES \"doc_person\" (\"id\") ON DELETE RESTRICT;"
                .to_owned()
        ),
        "{text:#?}"
    );
    assert!(text.contains(&"CREATE INDEX ON \"doc_invoice_item\" (\"parent_id\");".to_owned()));
    drop(client);
    assert_eq!(
        db.rows("SELECT coalesce(to_regclass('doc_invoice_item')::text, 'none')")
            .await[0][0],
        "none"
    );
}

// ---------------------------------------------------------------------------------------------
// Documents with rows
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn insert_with_rows_and_get_return_the_rows_in_order() {
    let (db, reg) = setup().await;
    let mut client = db.pool.get().await.unwrap();
    insert(&mut **client, &reg, "person", person("sam@x.y"), "cli")
        .await
        .unwrap();
    let mut second = item(2, "second");
    second.insert(
        "rate".into(),
        Value::Decimal("12.50".parse::<Decimal>().unwrap()),
    );
    second.insert("supplier".into(), text("sam@x.y"));
    // The third row omits every optional field.
    let third = BTreeMap::from([("qty".to_owned(), Value::Int(3))]);
    let rows = vec![item(1, "first"), second, third];
    let created = insert(
        &mut **client,
        &reg,
        "invoice",
        invoice("INV-1", rows),
        "cli",
    )
    .await
    .unwrap();

    assert_eq!(created.version, 1);
    assert_eq!(created.values["title"], text("INV-1"));
    assert!(!created.values.contains_key("items"));
    let items = &created.tables["items"];
    assert_eq!(items.len(), 3);
    assert_eq!(descriptions(&items[..2]), ["first", "second"]);
    assert_eq!(items[1]["rate"], Value::Decimal("12.50".parse().unwrap()));
    // Links come back as names; absent fields are null; every row has every field.
    assert_eq!(items[1]["supplier"], text("sam@x.y"));
    assert_eq!(items[0]["supplier"], Value::Null);
    assert_eq!(items[2]["description"], Value::Null);
    assert_eq!(items[2]["qty"], Value::Int(3));
    assert!(items.iter().all(|r| r.len() == 4));

    let fetched = get(&mut **client, &reg, "invoice", &created.name)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(fetched, created);

    // Stored as: row ids (UUID v7, all different), the parent's id, idx 1, 2, 3.
    let stored = db
        .rows(
            "SELECT i.idx::text, (i.parent_id = p.id)::text, (i.id <> p.id)::text \
             FROM doc_invoice_item i, doc_invoice p ORDER BY i.idx",
        )
        .await;
    assert_eq!(
        stored,
        [
            ["1", "true", "true"],
            ["2", "true", "true"],
            ["3", "true", "true"]
        ]
    );
    assert_eq!(
        db.rows("SELECT count(DISTINCT id)::text FROM doc_invoice_item")
            .await[0][0],
        "3"
    );
}

#[tokio::test]
async fn a_document_without_rows_has_an_empty_list() {
    let (db, reg) = setup().await;
    let mut client = db.pool.get().await.unwrap();
    let mut doc = invoice("empty", vec![]);
    doc.tables.clear();
    let created = insert(&mut **client, &reg, "invoice", doc, "cli")
        .await
        .unwrap();
    assert_eq!(created.tables["items"], Vec::<Row>::new());
    let fetched = get(&mut **client, &reg, "invoice", &created.name)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(fetched.tables["items"], Vec::<Row>::new());
    assert!(
        get(&mut **client, &reg, "invoice", "nothing")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn row_validation_errors_carry_positions_and_write_nothing() {
    let (db, reg) = setup().await;
    let mut client = db.pool.get().await.unwrap();
    insert(&mut **client, &reg, "person", person("sam@x.y"), "cli")
        .await
        .unwrap();
    let mut linked = item(2, "b");
    linked.insert("supplier".into(), text("nobody@x.y"));
    let mut bad_value = item(3, "c");
    bad_value.insert("qty".into(), text("three"));
    let rows = vec![
        item(1, "a"),
        linked,
        bad_value,
        BTreeMap::from([("description".to_owned(), text("no qty"))]),
    ];
    let error = insert(&mut **client, &reg, "invoice", invoice("X", rows), "cli")
        .await
        .unwrap_err();
    let DbError::Validation(errors) = &error else {
        panic!("{error}");
    };
    let in_row = |row: usize, error: ValidationError| ValidationError::InRow {
        field: "items".into(),
        row,
        error: Box::new(error),
    };
    assert!(errors.contains(&in_row(
        2,
        ValidationError::LinkNotFound {
            field: "supplier".into(),
            target: "person".into(),
            name: "nobody@x.y".into()
        }
    )));
    assert!(errors.contains(&in_row(
        3,
        ValidationError::TypeMismatch {
            field: "qty".into(),
            expected: "int",
            found: "text"
        }
    )));
    assert!(errors.contains(&in_row(
        4,
        ValidationError::Required {
            field: "qty".into()
        }
    )));
    assert_eq!(errors.len(), 3, "{errors:?}");
    assert!(
        error
            .to_string()
            .contains("field `items` row 2: field `supplier`: person `nobody@x.y` does not exist"),
        "{error}"
    );
    assert_eq!(count(&db, "doc_invoice").await, 0);
    assert_eq!(count(&db, "doc_invoice_item").await, 0);
}

#[tokio::test]
async fn a_required_table_needs_rows() {
    let db = TestDb::migrated().await;
    let reg = invoice_registry(true);
    db.sync(&reg).await.unwrap();
    let mut client = db.pool.get().await.unwrap();
    for doc in [invoice("none", vec![]), {
        let mut d = invoice("absent", vec![]);
        d.tables.clear();
        d
    }] {
        let error = insert(&mut **client, &reg, "invoice", doc, "cli")
            .await
            .unwrap_err();
        assert!(
            matches!(&error, DbError::Validation(e) if e == &[ValidationError::Required { field: "items".into() }]),
            "{error}"
        );
    }
    assert_eq!(count(&db, "doc_invoice").await, 0);
    let created = insert(
        &mut **client,
        &reg,
        "invoice",
        invoice("ok", vec![item(1, "a")]),
        "cli",
    )
    .await
    .unwrap();
    // An update may not remove the last rows either.
    let error = update(
        &mut **client,
        &reg,
        "invoice",
        &created.name,
        1,
        &rows_change(vec![]),
        "cli",
    )
    .await
    .unwrap_err();
    assert!(matches!(error, DbError::Validation(_)), "{error}");
    assert_eq!(count(&db, "doc_invoice_item").await, 1);
}

#[tokio::test]
async fn unknown_table_and_misused_table_values_are_validation_errors() {
    let (db, reg) = setup().await;
    let mut client = db.pool.get().await.unwrap();
    let mut doc = invoice("x", vec![]);
    doc.tables.insert("ghosts".into(), vec![]);
    doc.values.insert("items".into(), Value::Int(1));
    doc.tables.insert("title".into(), vec![item(1, "a")]);
    let error = insert(&mut **client, &reg, "invoice", doc, "cli")
        .await
        .unwrap_err();
    let DbError::Validation(errors) = &error else {
        panic!("{error}");
    };
    assert_eq!(errors.len(), 3, "{errors:?}");
    assert!(errors.contains(&ValidationError::UnknownField {
        field: "ghosts".into()
    }));
    assert!(errors.contains(&ValidationError::TypeMismatch {
        field: "items".into(),
        expected: "rows",
        found: "int"
    }));
    assert_eq!(count(&db, "doc_invoice").await, 0);
}

#[tokio::test]
async fn update_replaces_rows_and_keeps_them_when_not_mentioned() {
    let (db, reg) = setup().await;
    let mut client = db.pool.get().await.unwrap();
    let created = insert(
        &mut **client,
        &reg,
        "invoice",
        invoice("INV", vec![item(1, "a"), item(2, "b"), item(3, "c")]),
        "alice",
    )
    .await
    .unwrap();
    let old_ids = db
        .rows("SELECT id::text FROM doc_invoice_item ORDER BY idx")
        .await;

    // Replace with two rows in another order: count and order follow the new list.
    let v2 = update(
        &mut **client,
        &reg,
        "invoice",
        &created.name,
        1,
        &rows_change(vec![item(9, "z"), item(8, "y")]),
        "bob",
    )
    .await
    .unwrap();
    assert_eq!(v2.version, 2);
    assert_eq!(v2.modified_by, "bob");
    assert_eq!(v2.values["title"], text("INV"));
    assert_eq!(descriptions(&v2.tables["items"]), ["z", "y"]);
    assert_eq!(count(&db, "doc_invoice_item").await, 2);
    // The rows differ, so the old rows were deleted and new ones inserted (new ids).
    let new_ids = db
        .rows("SELECT id::text FROM doc_invoice_item ORDER BY idx")
        .await;
    assert!(new_ids.iter().all(|id| !old_ids.contains(id)));
    assert_eq!(
        db.rows("SELECT idx::text FROM doc_invoice_item ORDER BY idx")
            .await,
        [["1"], ["2"]]
    );

    // A values-only update leaves the rows alone.
    let changes = Changes {
        values: BTreeMap::from([("title".to_owned(), text("INV-2"))]),
        tables: BTreeMap::new(),
    };
    let v3 = update(
        &mut **client,
        &reg,
        "invoice",
        &created.name,
        2,
        &changes,
        "bob",
    )
    .await
    .unwrap();
    assert_eq!(v3.version, 3);
    assert_eq!(v3.tables, v2.tables);
    assert_eq!(
        db.rows("SELECT id::text FROM doc_invoice_item ORDER BY idx")
            .await,
        new_ids,
        "untouched rows keep their ids"
    );

    // Values and rows in one update.
    let both = Changes {
        values: BTreeMap::from([("title".to_owned(), text("INV-3"))]),
        tables: BTreeMap::from([("items".to_owned(), vec![item(5, "only")])]),
    };
    let v4 = update(
        &mut **client,
        &reg,
        "invoice",
        &created.name,
        3,
        &both,
        "bob",
    )
    .await
    .unwrap();
    assert_eq!(v4.values["title"], text("INV-3"));
    assert_eq!(descriptions(&v4.tables["items"]), ["only"]);

    // An empty list removes every row, and an update of the table alone is a valid change.
    let v5 = update(
        &mut **client,
        &reg,
        "invoice",
        &created.name,
        4,
        &rows_change(vec![]),
        "bob",
    )
    .await
    .unwrap();
    assert_eq!(v5.version, 5);
    assert_eq!(v5.tables["items"], Vec::<Row>::new());
    assert_eq!(count(&db, "doc_invoice_item").await, 0);

    // Nothing at all to change is still an error.
    let error = update(
        &mut **client,
        &reg,
        "invoice",
        &created.name,
        5,
        &Changes::default(),
        "bob",
    )
    .await
    .unwrap_err();
    assert!(matches!(error, DbError::NoChanges), "{error}");
}

#[tokio::test]
async fn update_with_a_stale_version_or_bad_rows_leaves_the_rows_alone() {
    let (db, reg) = setup().await;
    let mut client = db.pool.get().await.unwrap();
    let created = insert(
        &mut **client,
        &reg,
        "invoice",
        invoice("INV", vec![item(1, "keep")]),
        "cli",
    )
    .await
    .unwrap();
    let error = update(
        &mut **client,
        &reg,
        "invoice",
        &created.name,
        7,
        &rows_change(vec![item(2, "lost")]),
        "cli",
    )
    .await
    .unwrap_err();
    assert!(
        matches!(
            error,
            DbError::Conflict {
                expected: 7,
                current: 1,
                ..
            }
        ),
        "{error}"
    );
    let error = update(
        &mut **client,
        &reg,
        "invoice",
        &created.name,
        1,
        &rows_change(vec![item(2, "ok"), BTreeMap::new()]),
        "cli",
    )
    .await
    .unwrap_err();
    assert!(matches!(error, DbError::Validation(_)), "{error}");
    let error = update(
        &mut **client,
        &reg,
        "invoice",
        "nothing",
        1,
        &rows_change(vec![item(2, "ok")]),
        "cli",
    )
    .await
    .unwrap_err();
    assert!(matches!(error, DbError::NotFound { .. }), "{error}");
    let fetched = get(&mut **client, &reg, "invoice", &created.name)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(fetched, created);
}

#[tokio::test]
async fn delete_removes_the_rows_with_the_document() {
    let (db, reg) = setup().await;
    let mut client = db.pool.get().await.unwrap();
    let a = insert(
        &mut **client,
        &reg,
        "invoice",
        invoice("A", vec![item(1, "a1"), item(2, "a2")]),
        "cli",
    )
    .await
    .unwrap();
    insert(
        &mut **client,
        &reg,
        "invoice",
        invoice("B", vec![item(1, "b1")]),
        "cli",
    )
    .await
    .unwrap();
    assert_eq!(count(&db, "doc_invoice_item").await, 3);
    // A stale delete removes nothing.
    assert!(
        delete(&mut **client, &reg, "invoice", &a.name, 9, "ann")
            .await
            .is_err()
    );
    assert_eq!(count(&db, "doc_invoice_item").await, 3);
    delete(&mut **client, &reg, "invoice", &a.name, 1, "ann")
        .await
        .unwrap();
    assert_eq!(count(&db, "doc_invoice").await, 1);
    assert_eq!(
        db.rows("SELECT description::text FROM doc_invoice_item")
            .await,
        [["b1"]]
    );
}

#[tokio::test]
async fn a_person_linked_from_a_row_cannot_be_deleted_and_the_error_names_the_owner() {
    let (db, reg) = setup().await;
    let mut client = db.pool.get().await.unwrap();
    let sam = insert(&mut **client, &reg, "person", person("sam@x.y"), "cli")
        .await
        .unwrap();
    let mut row = item(1, "a");
    row.insert("supplier".into(), text("sam@x.y"));
    let created = insert(
        &mut **client,
        &reg,
        "invoice",
        invoice("INV", vec![row]),
        "cli",
    )
    .await
    .unwrap();
    let error = delete(&mut **client, &reg, "person", &sam.name, 1, "ann")
        .await
        .unwrap_err();
    assert!(
        matches!(&error, DbError::LinkedDocumentExists { referenced_by: Some(by), .. } if by == "invoice"),
        "{error}"
    );
    // Once the invoice (and so its rows) is gone, the person can be deleted.
    delete(&mut **client, &reg, "invoice", &created.name, 1, "ann")
        .await
        .unwrap();
    delete(&mut **client, &reg, "person", &sam.name, 1, "ann")
        .await
        .unwrap();
}

#[tokio::test]
async fn child_doctypes_have_no_crud_of_their_own() {
    let (db, reg) = setup().await;
    let mut client = db.pool.get().await.unwrap();
    let is_child =
        |e: &DbError| matches!(e, DbError::ChildDocType { doctype } if doctype == "invoice_item");
    let doc = Document {
        name: None,
        values: BTreeMap::from([("qty".to_owned(), Value::Int(1))]),
        tables: BTreeMap::new(),
    };
    let error = insert(&mut **client, &reg, "invoice_item", doc, "cli")
        .await
        .unwrap_err();
    assert!(is_child(&error), "{error}");
    assert!(error.to_string().contains("child DocType"), "{error}");
    let error = get(&mut **client, &reg, "invoice_item", "x")
        .await
        .unwrap_err();
    assert!(is_child(&error), "{error}");
    let changes = Changes {
        values: BTreeMap::from([("qty".to_owned(), Value::Int(1))]),
        tables: BTreeMap::new(),
    };
    let error = update(&mut **client, &reg, "invoice_item", "x", 1, &changes, "cli")
        .await
        .unwrap_err();
    assert!(is_child(&error), "{error}");
    let error = delete(&mut **client, &reg, "invoice_item", "x", 1, "ann")
        .await
        .unwrap_err();
    assert!(is_child(&error), "{error}");
    let error = get(&mut **client, &reg, "nope", "x").await.unwrap_err();
    assert!(
        matches!(&error, DbError::UnknownDocType { name } if name == "nope"),
        "{error}"
    );
    assert_eq!(count(&db, "doc_invoice_item").await, 0);
}

// ---------------------------------------------------------------------------------------------
// Atomicity
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_failing_row_write_leaves_no_parent_on_a_pooled_client() {
    let (db, reg) = setup().await;
    // Rows with qty 13 pass validation but the database refuses them: the parent row is
    // inserted first, so only a transaction around both statements keeps it out.
    db.exec("ALTER TABLE doc_invoice_item ADD CONSTRAINT no_thirteen CHECK (qty <> 13)")
        .await;
    let mut client = db.pool.get().await.unwrap();
    let error = insert(
        &mut **client,
        &reg,
        "invoice",
        invoice("INV", vec![item(1, "fine"), item(13, "unlucky")]),
        "cli",
    )
    .await
    .unwrap_err();
    assert!(
        matches!(&error, DbError::Postgres(e) if e.code().map(|c| c.code()) == Some("23514")),
        "{error}"
    );
    assert_eq!(count(&db, "doc_invoice").await, 0);
    assert_eq!(count(&db, "doc_invoice_item").await, 0);

    // The same for an update: the old rows and the old version stay.
    let created = insert(
        &mut **client,
        &reg,
        "invoice",
        invoice("INV", vec![item(1, "old")]),
        "cli",
    )
    .await
    .unwrap();
    let changes = Changes {
        values: BTreeMap::from([("title".to_owned(), text("changed"))]),
        tables: BTreeMap::from([("items".to_owned(), vec![item(2, "new"), item(13, "boom")])]),
    };
    update(
        &mut **client,
        &reg,
        "invoice",
        &created.name,
        1,
        &changes,
        "cli",
    )
    .await
    .unwrap_err();
    let fetched = get(&mut **client, &reg, "invoice", &created.name)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(fetched, created);
}

#[tokio::test]
async fn operations_in_a_caller_transaction_roll_back_with_it_and_a_failure_does_not_abort_it() {
    let (db, reg) = setup().await;
    db.exec("ALTER TABLE doc_invoice_item ADD CONSTRAINT no_thirteen CHECK (qty <> 13)")
        .await;
    let mut client = db.pool.get().await.unwrap();
    let raw: &mut forgebase_db::Client = &mut client;

    let mut tx = raw.transaction().await.unwrap();
    insert(
        &mut tx,
        &reg,
        "invoice",
        invoice("kept", vec![item(1, "a")]),
        "cli",
    )
    .await
    .unwrap();
    // This call fails on the database after its parent row was written ...
    insert(
        &mut tx,
        &reg,
        "invoice",
        invoice("lost", vec![item(13, "b")]),
        "cli",
    )
    .await
    .unwrap_err();
    // ... but only its own work is undone: the transaction is usable and still has the first.
    let rows = tx
        .query("SELECT title::text FROM doc_invoice ORDER BY 1", &[])
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    tx.commit().await.unwrap();
    assert_eq!(count(&db, "doc_invoice").await, 1);
    assert_eq!(count(&db, "doc_invoice_item").await, 1);

    let mut tx = raw.transaction().await.unwrap();
    insert(
        &mut tx,
        &reg,
        "invoice",
        invoice("rolled back", vec![item(2, "c")]),
        "cli",
    )
    .await
    .unwrap();
    tx.rollback().await.unwrap();
    assert_eq!(count(&db, "doc_invoice").await, 1);
    assert_eq!(count(&db, "doc_invoice_item").await, 1);
}

#[tokio::test]
async fn a_unique_violation_in_the_parent_writes_no_rows() {
    let db = TestDb::migrated().await;
    let invoice_unique = invoice_toml(false).replace(
        "name = \"title\"\ntype = \"data\"\n",
        "name = \"title\"\ntype = \"data\"\nunique = true\n",
    );
    let reg = registry(&[PERSON, &invoice_unique, INVOICE_ITEM]);
    db.sync(&reg).await.unwrap();
    let mut client = db.pool.get().await.unwrap();
    insert(
        &mut **client,
        &reg,
        "invoice",
        invoice("same", vec![item(1, "a")]),
        "cli",
    )
    .await
    .unwrap();
    let error = insert(
        &mut **client,
        &reg,
        "invoice",
        invoice("same", vec![item(2, "b")]),
        "cli",
    )
    .await
    .unwrap_err();
    assert!(matches!(error, DbError::UniqueViolation { .. }), "{error}");
    assert_eq!(count(&db, "doc_invoice_item").await, 1);
}

// ---------------------------------------------------------------------------------------------
// Concurrency
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_read_never_mixes_a_document_with_the_rows_of_another_version() {
    let (db, reg) = setup().await;
    let mut writer = db.pool.get().await.unwrap();
    let mut reader = db.pool.get().await.unwrap();
    let created = insert(
        &mut **writer,
        &reg,
        "invoice",
        invoice("INV", vec![item(1, "v1")]),
        "cli",
    )
    .await
    .unwrap();
    let name = created.name.clone();

    // Every version of the document has exactly one row, "v<version>": a reader that sees
    // version N together with another version's row has read two states.
    let write = async {
        for version in 1..=40_i64 {
            update(
                &mut **writer,
                &reg,
                "invoice",
                &name,
                version,
                &rows_change(vec![item(1, &format!("v{}", version + 1))]),
                "cli",
            )
            .await
            .unwrap();
        }
    };
    let read = async {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        let mut seen = 0;
        while seen < 40 && tokio::time::Instant::now() < deadline {
            match get(&mut **reader, &reg, "invoice", &name).await {
                Ok(Some(doc)) => {
                    let rows = &doc.tables["items"];
                    assert_eq!(rows.len(), 1);
                    assert_eq!(
                        descriptions(rows),
                        [format!("v{}", doc.version)],
                        "version {} with another version's rows",
                        doc.version
                    );
                    seen = doc.version;
                }
                Ok(None) => panic!("the document disappeared"),
                // Allowed when a writer keeps winning the race; the data is never mixed.
                Err(DbError::ChangedWhileReading { .. }) => {}
                Err(other) => panic!("{other}"),
            }
        }
    };
    tokio::join!(write, read);
}

#[tokio::test]
async fn concurrent_row_replacements_with_one_version_have_one_winner() {
    let (db, reg) = setup().await;
    let (mut a, mut b) = (db.pool.get().await.unwrap(), db.pool.get().await.unwrap());
    let created = insert(
        &mut **a,
        &reg,
        "invoice",
        invoice("INV", vec![item(1, "start")]),
        "cli",
    )
    .await
    .unwrap();
    let (one, two) = (
        rows_change(vec![item(1, "one-a"), item(2, "one-b")]),
        rows_change(vec![item(3, "two-a")]),
    );
    let (ra, rb) = tokio::join!(
        update(&mut **a, &reg, "invoice", &created.name, 1, &one, "a"),
        update(&mut **b, &reg, "invoice", &created.name, 1, &two, "b"),
    );
    let (winner, loser) = match (ra, rb) {
        (Ok(w), Err(l)) | (Err(l), Ok(w)) => (w, l),
        other => panic!("exactly one update should win: {other:?}"),
    };
    assert!(
        matches!(
            loser,
            DbError::Conflict {
                expected: 1,
                current: 2,
                ..
            }
        ),
        "{loser}"
    );
    let fetched = get(&mut **a, &reg, "invoice", &created.name)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(fetched, winner);
    assert_eq!(
        fetched.tables["items"].len(),
        count(&db, "doc_invoice_item").await as usize
    );
}
