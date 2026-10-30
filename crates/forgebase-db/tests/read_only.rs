//! `read_only` fields (UX.1) against a real PostgreSQL (see `common`): a caller's input for them
//! is refused before the hooks run, a hook may set them, and an amendment is not caller input.

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;

use common::{TestDb, registry};
use forgebase_core::document::{Changes, Document, Row, ValidationError};
use forgebase_core::hooks::{DocTypeHooks, HookContext, HookError};
use forgebase_core::meta::Registry;
use forgebase_core::permission::Caller;
use forgebase_core::value::Value;
use forgebase_db::{DbError, StoredDocument, amend, cancel, get, insert, submit, update};

const SHEET: &str = r#"
name = "sheet"
label = "Sheet"
submittable = true
[[fields]]
name = "title"
type = "data"
required = true
[[fields]]
name = "total"
type = "int"
read_only = true
[[fields]]
name = "lines"
type = "table"
target = "sheet_line"
"#;

const LINE: &str = r#"
name = "sheet_line"
label = "Sheet Line"
is_child = true
[[fields]]
name = "qty"
type = "int"
required = true
[[fields]]
name = "amount"
type = "int"
read_only = true
"#;

/// Computes `amount` (qty * 10) of every row and `total` (their sum), like an app's hook.
struct Compute;

impl DocTypeHooks for Compute {
    fn validate(&self, _ctx: &HookContext<'_>, doc: &mut Document) -> Result<(), HookError> {
        let mut total = 0;
        for row in doc.tables.entry("lines".into()).or_default() {
            if let Some(Value::Int(qty)) = row.get("qty") {
                let amount = qty * 10;
                total += amount;
                row.insert("amount".into(), Value::Int(amount));
            }
        }
        doc.values.insert("total".into(), Value::Int(total));
        Ok(())
    }
}

fn plain() -> Registry {
    registry(&[SHEET, LINE])
}

fn computing() -> Registry {
    let mut reg = plain();
    reg.add_hooks("sheet", Arc::new(Compute)).expect("hooks");
    reg
}

fn line(qty: i64, amount: Option<i64>) -> Row {
    let mut row = BTreeMap::from([("qty".to_owned(), Value::Int(qty))]);
    if let Some(amount) = amount {
        row.insert("amount".to_owned(), Value::Int(amount));
    }
    row
}

fn sheet(total: Option<Value>, lines: Vec<Row>) -> Document {
    let mut values = BTreeMap::from([("title".to_owned(), Value::Text("s".into()))]);
    if let Some(total) = total {
        values.insert("total".to_owned(), total);
    }
    Document {
        name: None,
        values,
        tables: BTreeMap::from([("lines".to_owned(), lines)]),
    }
}

fn ann() -> Caller {
    Caller::system("ann")
}

fn read_only(field: &str) -> ValidationError {
    ValidationError::ReadOnly {
        field: field.into(),
    }
}

fn in_row(row: usize, field: &str) -> ValidationError {
    ValidationError::InRow {
        field: "lines".into(),
        row,
        error: Box::new(read_only(field)),
    }
}

fn refused(result: Result<StoredDocument, DbError>) -> Vec<ValidationError> {
    match result {
        Err(DbError::Validation(errors)) => errors,
        other => panic!("expected a validation error, got {other:?}"),
    }
}

async fn setup(reg: &Registry) -> TestDb {
    let db = TestDb::migrated().await;
    db.sync(reg).await.expect("sync");
    db
}

async fn count(db: &TestDb, table: &str) -> i64 {
    db.rows(&format!("SELECT count(*)::text FROM {table}"))
        .await[0][0]
        .parse()
        .expect("count")
}

#[tokio::test]
async fn insert_refuses_caller_values_top_level_and_in_rows_and_writes_nothing() {
    let reg = plain();
    let db = setup(&reg).await;
    let mut client = db.pool.get().await.unwrap();
    let doc = sheet(
        Some(Value::Int(5)),
        vec![line(1, Some(2)), line(2, None), line(3, Some(9))],
    );
    let errors = refused(insert(&mut **client, &reg, "sheet", doc, &ann()).await);
    assert_eq!(
        errors,
        [read_only("total"), in_row(1, "amount"), in_row(3, "amount")]
    );
    assert_eq!(count(&db, "doc_sheet").await, 0);

    // Null, or absent, is not a value.
    let doc = sheet(Some(Value::Null), vec![line(1, None)]);
    let stored = insert(&mut **client, &reg, "sheet", doc, &ann())
        .await
        .unwrap();
    assert_eq!(stored.values["total"], Value::Null);
}

#[tokio::test]
async fn the_refusal_comes_before_the_hooks_run() {
    // The hook would overwrite the value, but the caller's input is still refused.
    let reg = computing();
    let db = setup(&reg).await;
    let mut client = db.pool.get().await.unwrap();
    let doc = sheet(Some(Value::Int(1)), vec![line(1, None)]);
    let errors = refused(insert(&mut **client, &reg, "sheet", doc, &ann()).await);
    assert_eq!(errors, [read_only("total")]);
}

#[tokio::test]
async fn a_hook_sets_read_only_fields_on_insert_and_update() {
    let reg = computing();
    let db = setup(&reg).await;
    let mut client = db.pool.get().await.unwrap();
    let c = &mut **client;
    let stored = insert(
        c,
        &reg,
        "sheet",
        sheet(None, vec![line(1, None), line(2, None)]),
        &ann(),
    )
    .await
    .unwrap();
    assert_eq!(stored.values["total"], Value::Int(30));
    assert_eq!(stored.tables["lines"][1]["amount"], Value::Int(20));

    // Replacing the table without the computed column: the hook fills it in and it is stored.
    let changes = Changes {
        values: BTreeMap::new(),
        tables: BTreeMap::from([("lines".to_owned(), vec![line(4, None)])]),
    };
    let stored = update(
        c,
        &reg,
        "sheet",
        &stored.name,
        stored.version,
        &changes,
        &ann(),
    )
    .await
    .unwrap();
    assert_eq!(stored.values["total"], Value::Int(40));
    assert_eq!(stored.tables["lines"][0]["amount"], Value::Int(40));
    let read = get(c, &reg, "sheet", &stored.name, &ann())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(read.values["total"], Value::Int(40));
}

#[tokio::test]
async fn update_refuses_a_changed_value_and_a_row_value_but_accepts_the_stored_value() {
    let reg = computing();
    let db = setup(&reg).await;
    let mut client = db.pool.get().await.unwrap();
    let c = &mut **client;
    let stored = insert(c, &reg, "sheet", sheet(None, vec![line(1, None)]), &ann())
        .await
        .unwrap();
    let (name, version) = (stored.name.clone(), stored.version);
    let set = |total: i64, rows: Option<Vec<Row>>| Changes {
        values: BTreeMap::from([("total".to_owned(), Value::Int(total))]),
        tables: rows
            .map(|rows| BTreeMap::from([("lines".to_owned(), rows)]))
            .unwrap_or_default(),
    };

    let errors = refused(update(c, &reg, "sheet", &name, version, &set(99, None), &ann()).await);
    assert_eq!(errors, [read_only("total")]);

    // A row with a computed value is refused even when it equals the stored one.
    let rows = Some(vec![line(1, Some(10))]);
    let errors = refused(update(c, &reg, "sheet", &name, version, &set(10, rows), &ann()).await);
    assert_eq!(errors, [in_row(1, "amount")]);

    // Sending the stored value back is no change.
    let stored = update(c, &reg, "sheet", &name, version, &set(10, None), &ann())
        .await
        .unwrap();
    assert_eq!(
        (stored.version, &stored.values["total"]),
        (2, &Value::Int(10))
    );
}

#[tokio::test]
async fn amend_copies_read_only_values_without_refusing_them() {
    let reg = computing();
    let db = setup(&reg).await;
    let mut client = db.pool.get().await.unwrap();
    let c = &mut **client;
    let stored = insert(c, &reg, "sheet", sheet(None, vec![line(2, None)]), &ann())
        .await
        .unwrap();
    let stored = submit(c, &reg, "sheet", &stored.name, stored.version, &ann())
        .await
        .unwrap();
    let stored = cancel(c, &reg, "sheet", &stored.name, stored.version, &ann())
        .await
        .unwrap();
    assert_eq!(stored.values["total"], Value::Int(20));
    let amended = amend(c, &reg, "sheet", &stored.name, stored.version, &ann())
        .await
        .unwrap();
    assert_eq!(amended.amended_from.as_deref(), Some(stored.name.as_str()));
    assert_eq!(amended.values["total"], Value::Int(20));
    assert_eq!(amended.tables["lines"][0]["amount"], Value::Int(20));
}
