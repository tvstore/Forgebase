//! The lifecycle pipeline and hooks (M2.3) against a real PostgreSQL (see `common`): the order
//! of the hooks and the framework steps, what a hook may change, and the rollback on a hook
//! error.

mod common;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{TestDb, registry};
use forgebase_core::document::{Changes, Document, Row, ValidationError};
use forgebase_core::hooks::{DocTypeHooks, HookContext, HookError, Operation};
use forgebase_core::meta::Registry;
use forgebase_core::permission::Caller;
use forgebase_core::value::Value;
use forgebase_db::{DbError, StoredDocument, delete, get, insert, update};

const INVOICE: &str = r#"
name = "invoice"
label = "Invoice"
[[fields]]
name = "title"
type = "data"
required = true
[[fields]]
name = "note"
type = "text"
[[fields]]
name = "amount"
type = "int"
[[fields]]
name = "items"
type = "table"
target = "invoice_item"
"#;

const ITEM: &str = r#"
name = "invoice_item"
label = "Invoice Item"
is_child = true
[[fields]]
name = "qty"
type = "int"
required = true
[[fields]]
name = "description"
type = "data"
"#;

const ACCOUNT: &str = r#"
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

type Log = Arc<Mutex<Vec<String>>>;

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

fn set(field: &str, value: Value) -> Changes {
    Changes {
        values: BTreeMap::from([(field.to_owned(), value)]),
        tables: BTreeMap::new(),
    }
}

fn taken(log: &Log) -> Vec<String> {
    std::mem::take(&mut *log.lock().expect("log"))
}

/// Writes `<id>:<hook>:<operation>` to the log; fails at the one hook it was told to.
struct Recorder {
    id: &'static str,
    log: Log,
    fail_at: Option<(&'static str, Operation)>,
}

impl Recorder {
    fn record(&self, hook: &'static str, ctx: &HookContext<'_>) -> Result<(), HookError> {
        self.log
            .lock()
            .expect("log")
            .push(format!("{}:{hook}:{:?}", self.id, ctx.operation));
        if self.fail_at == Some((hook, ctx.operation)) {
            return Err(HookError::new(format!("{} says no", self.id)));
        }
        Ok(())
    }
}

impl DocTypeHooks for Recorder {
    fn before_insert(&self, ctx: &HookContext<'_>, _doc: &mut Document) -> Result<(), HookError> {
        self.record("before_insert", ctx)
    }
    fn validate(&self, ctx: &HookContext<'_>, _doc: &mut Document) -> Result<(), HookError> {
        self.record("validate", ctx)
    }
    fn before_delete(&self, ctx: &HookContext<'_>, _doc: &Document) -> Result<(), HookError> {
        self.record("before_delete", ctx)
    }
}

/// Registry of the invoice DocTypes with two recorders (`A` registered first) and their log.
fn recording(fail_at: Option<(&'static str, Operation)>) -> (Registry, Log) {
    let log = Log::default();
    let mut reg = registry(&[INVOICE, ITEM]);
    for (id, fail) in [("A", fail_at), ("B", None)] {
        let hooks = Recorder {
            id,
            log: log.clone(),
            fail_at: fail,
        };
        reg.add_hooks("invoice", Arc::new(hooks)).expect("hooks");
    }
    (reg, log)
}

async fn setup() -> (TestDb, Registry) {
    let db = TestDb::migrated().await;
    let reg = registry(&[INVOICE, ITEM, ACCOUNT]);
    db.sync(&reg).await.expect("sync");
    (db, reg)
}

async fn count(db: &TestDb, table: &str) -> i64 {
    db.rows(&format!("SELECT count(*)::text FROM {table}"))
        .await[0][0]
        .parse()
        .expect("count")
}

async fn item_ids(db: &TestDb) -> Vec<String> {
    db.rows("SELECT id::text FROM doc_invoice_item ORDER BY idx")
        .await
        .into_iter()
        .map(|mut r| r.remove(0))
        .collect()
}

fn titles(stored: &StoredDocument) -> (Value, Value) {
    (
        stored.values["title"].clone(),
        stored.values["note"].clone(),
    )
}

#[tokio::test]
async fn insert_update_and_delete_run_the_hooks_in_the_documented_order() {
    let (db, _) = setup().await;
    let (reg, log) = recording(None);
    let mut client = db.pool.get().await.unwrap();

    let stored = insert(
        &mut **client,
        &reg,
        "invoice",
        invoice("a", vec![]),
        &Caller::system("ann"),
    )
    .await
    .unwrap();
    // Per hook point in registration order: A, then B; before_insert before validate.
    assert_eq!(
        taken(&log),
        [
            "A:before_insert:Insert",
            "B:before_insert:Insert",
            "A:validate:Insert",
            "B:validate:Insert"
        ]
    );

    update(
        &mut **client,
        &reg,
        "invoice",
        &stored.name,
        1,
        &set("note", text("n")),
        &Caller::system("ann"),
    )
    .await
    .unwrap();
    assert_eq!(taken(&log), ["A:validate:Update", "B:validate:Update"]);

    delete(
        &mut **client,
        &reg,
        "invoice",
        &stored.name,
        2,
        &Caller::system("ann"),
    )
    .await
    .unwrap();
    assert_eq!(
        taken(&log),
        ["A:before_delete:Delete", "B:before_delete:Delete"]
    );
    assert_eq!(count(&db, "doc_invoice").await, 0);
}

#[tokio::test]
async fn hooks_run_before_framework_validation_and_it_runs_again_after_them() {
    let (db, _) = setup().await;
    let (reg, log) = recording(None);
    let mut client = db.pool.get().await.unwrap();
    // No title: the hooks still run (they see the input as given) ...
    let error = insert(
        &mut **client,
        &reg,
        "invoice",
        Document::default(),
        &Caller::system("ann"),
    )
    .await
    .unwrap_err();
    assert_eq!(taken(&log).len(), 4);
    // ... and then the framework reports the problem.
    assert!(
        matches!(&error, DbError::Validation(e) if e == &[ValidationError::Required { field: "title".into() }]),
        "{error}"
    );
    assert_eq!(count(&db, "doc_invoice").await, 0);
}

#[tokio::test]
async fn hooks_get_the_context_and_see_the_whole_document_on_update() {
    /// What a hook was called with: doctype, actor, operation and the document.
    type Call = (String, String, Operation, Document);

    /// Keeps what it was called with.
    #[derive(Default)]
    struct Spy(Mutex<Vec<Call>>);

    impl Spy {
        fn see(&self, ctx: &HookContext<'_>, doc: &Document) {
            self.0.lock().expect("spy").push((
                ctx.doctype.to_owned(),
                ctx.actor.to_owned(),
                ctx.operation,
                doc.clone(),
            ));
        }
    }
    struct Share(Arc<Spy>);
    impl DocTypeHooks for Share {
        fn validate(&self, ctx: &HookContext<'_>, doc: &mut Document) -> Result<(), HookError> {
            self.0.see(ctx, doc);
            Ok(())
        }
        fn before_delete(&self, ctx: &HookContext<'_>, doc: &Document) -> Result<(), HookError> {
            self.0.see(ctx, doc);
            Ok(())
        }
    }

    let (db, _) = setup().await;
    let spy = Arc::new(Spy::default());
    let mut reg = registry(&[INVOICE, ITEM, ACCOUNT]);
    reg.add_hooks("invoice", Arc::new(Share(spy.clone())))
        .unwrap();
    let mut client = db.pool.get().await.unwrap();

    let first = invoice("one", vec![item(1, "a"), item(2, "b")]);
    let stored = insert(
        &mut **client,
        &reg,
        "invoice",
        first,
        &Caller::system("ann"),
    )
    .await
    .unwrap();
    // The changes name only the title: the hook still sees the stored note, amount and rows.
    let mut changes = set("title", text("two"));
    changes.values.insert("note".to_owned(), text("kept"));
    update(
        &mut **client,
        &reg,
        "invoice",
        &stored.name,
        1,
        &changes,
        &Caller::system("bob"),
    )
    .await
    .unwrap();
    changes = set("title", text("three"));
    update(
        &mut **client,
        &reg,
        "invoice",
        &stored.name,
        2,
        &changes,
        &Caller::system("cy"),
    )
    .await
    .unwrap();
    delete(
        &mut **client,
        &reg,
        "invoice",
        &stored.name,
        3,
        &Caller::system("dee"),
    )
    .await
    .unwrap();

    let seen = spy.0.lock().unwrap();
    assert_eq!(seen.len(), 4);
    let (doctype, actor, operation, doc) = &seen[0];
    assert_eq!(
        (doctype.as_str(), actor.as_str(), *operation),
        ("invoice", "ann", Operation::Insert)
    );
    assert_eq!(doc.name, None);
    let (_, actor, operation, doc) = &seen[1];
    assert_eq!((actor.as_str(), *operation), ("bob", Operation::Update));
    assert_eq!(doc.name.as_deref(), Some(stored.name.as_str()));
    assert_eq!(doc.values["title"], text("two"));
    assert_eq!(doc.values["note"], text("kept"));
    assert_eq!(doc.values["amount"], Value::Null);
    assert_eq!(doc.tables["items"].len(), 2);
    let (_, _, _, doc) = &seen[2];
    assert_eq!(doc.values["title"], text("three"));
    assert_eq!(
        doc.values["note"],
        text("kept"),
        "stored values are merged in"
    );
    assert_eq!(doc.tables["items"].len(), 2, "stored rows are merged in");
    // before_delete: the stored document with its rows; the actor is the one given to delete.
    let (_, actor, operation, doc) = &seen[3];
    assert_eq!((actor.as_str(), *operation), ("dee", Operation::Delete));
    assert_eq!(doc.values["title"], text("three"));
    assert_eq!(doc.tables["items"], vec![item(1, "a"), item(2, "b")]);
}

/// Sets `amount` before insert and `note` on every validate; adds a row for the title `grow`.
struct Mutator;

impl DocTypeHooks for Mutator {
    fn before_insert(&self, _ctx: &HookContext<'_>, doc: &mut Document) -> Result<(), HookError> {
        doc.values.insert("amount".to_owned(), Value::Int(7));
        Ok(())
    }
    fn validate(&self, _ctx: &HookContext<'_>, doc: &mut Document) -> Result<(), HookError> {
        doc.values.insert("note".to_owned(), text("hooked"));
        if doc.values.get("title") == Some(&text("grow")) {
            doc.tables
                .entry("items".to_owned())
                .or_default()
                .push(item(9, "from hook"));
        }
        Ok(())
    }
}

#[tokio::test]
async fn changes_made_by_hooks_are_persisted_on_insert_and_update() {
    let (db, _) = setup().await;
    let mut reg = registry(&[INVOICE, ITEM, ACCOUNT]);
    reg.add_hooks("invoice", Arc::new(Mutator)).unwrap();
    let mut client = db.pool.get().await.unwrap();

    let stored = insert(
        &mut **client,
        &reg,
        "invoice",
        invoice("plain", vec![item(1, "a"), item(2, "b")]),
        &Caller::system("ann"),
    )
    .await
    .unwrap();
    assert_eq!(stored.values["amount"], Value::Int(7));
    assert_eq!(stored.values["note"], text("hooked"));
    let again = get(
        &mut **client,
        &reg,
        "invoice",
        &stored.name,
        &Caller::system("cli"),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(again, stored);
    let ids = item_ids(&db).await;
    assert_eq!(ids.len(), 2);

    // The hook wins over the caller's value, for a field the changes name ...
    let updated = update(
        &mut **client,
        &reg,
        "invoice",
        &stored.name,
        1,
        &set("note", text("mine")),
        &Caller::system("ann"),
    )
    .await
    .unwrap();
    assert_eq!(updated.version, 2);
    assert_eq!(updated.values["note"], text("hooked"));
    // ... and a table the changes do not name, and the hook does not touch, is left alone.
    assert_eq!(item_ids(&db).await, ids);

    // A hook that adds a row to a table the changes do not name: the rows are replaced.
    let updated = update(
        &mut **client,
        &reg,
        "invoice",
        &stored.name,
        2,
        &set("title", text("grow")),
        &Caller::system("ann"),
    )
    .await
    .unwrap();
    assert_eq!(updated.version, 3);
    assert_eq!(
        updated.tables["items"],
        vec![item(1, "a"), item(2, "b"), item(9, "from hook")]
    );
    let new_ids = item_ids(&db).await;
    assert_eq!(new_ids.len(), 3);
    assert!(
        ids.iter().all(|id| !new_ids.contains(id)),
        "rows were rewritten"
    );
    assert_eq!(count(&db, "doc_invoice_item").await, 3);

    // An explicit save that changes nothing still counts: the version is bumped.
    let same = update(
        &mut **client,
        &reg,
        "invoice",
        &stored.name,
        3,
        &set("note", text("hooked")),
        &Caller::system("ann"),
    )
    .await
    .unwrap();
    assert_eq!(same.version, 4);
    assert_eq!(same.values, updated.values);
}

/// Makes the document invalid after the fact, depending on the note.
struct Breaker;

impl DocTypeHooks for Breaker {
    fn validate(&self, _ctx: &HookContext<'_>, doc: &mut Document) -> Result<(), HookError> {
        match doc.values.get("note") {
            Some(Value::Text(note)) if note == "clear title" => {
                doc.values.insert("title".to_owned(), Value::Null);
            }
            Some(Value::Text(note)) if note == "bad row" => {
                doc.tables
                    .entry("items".to_owned())
                    .or_default()
                    .push(Row::new());
            }
            Some(Value::Text(note)) if note == "unknown" => {
                doc.values.insert("bogus".to_owned(), Value::Int(1));
            }
            _ => {}
        }
        Ok(())
    }
}

#[tokio::test]
async fn a_hook_cannot_leave_an_invalid_document() {
    let (db, _) = setup().await;
    let mut reg = registry(&[INVOICE, ITEM, ACCOUNT]);
    reg.add_hooks("invoice", Arc::new(Breaker)).unwrap();
    let mut client = db.pool.get().await.unwrap();

    let with_note = |note: &str| {
        let mut doc = invoice("t", vec![item(1, "a")]);
        doc.values.insert("note".to_owned(), text(note));
        doc
    };
    for (note, expected) in [
        (
            "clear title",
            ValidationError::Required {
                field: "title".into(),
            },
        ),
        (
            "bad row",
            ValidationError::InRow {
                field: "items".into(),
                row: 2,
                error: Box::new(ValidationError::Required {
                    field: "qty".into(),
                }),
            },
        ),
        (
            "unknown",
            ValidationError::UnknownField {
                field: "bogus".into(),
            },
        ),
    ] {
        let error = insert(
            &mut **client,
            &reg,
            "invoice",
            with_note(note),
            &Caller::system("ann"),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(&error, DbError::Validation(e) if e == std::slice::from_ref(&expected)),
            "{note}: {error}"
        );
        assert_eq!(count(&db, "doc_invoice").await, 0, "{note}");
    }

    // The same on update; the stored document stays as it was.
    let stored = insert(
        &mut **client,
        &reg,
        "invoice",
        with_note("fine"),
        &Caller::system("ann"),
    )
    .await
    .unwrap();
    for note in ["clear title", "bad row", "unknown"] {
        let error = update(
            &mut **client,
            &reg,
            "invoice",
            &stored.name,
            1,
            &set("note", text(note)),
            &Caller::system("ann"),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(error, DbError::Validation(ref e) if e.len() == 1),
            "{note}: {error}"
        );
    }
    let after = get(
        &mut **client,
        &reg,
        "invoice",
        &stored.name,
        &Caller::system("cli"),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(after, stored);
}

#[tokio::test]
async fn a_failing_hook_rejects_the_operation_and_persists_nothing() {
    let (db, plain) = setup().await;
    let mut client = db.pool.get().await.unwrap();
    let stored = insert(
        &mut **client,
        &plain,
        "invoice",
        invoice("kept", vec![item(1, "a")]),
        &Caller::system("ann"),
    )
    .await
    .unwrap();
    let ids = item_ids(&db).await;

    // (hook, operation): A fails, so B never runs for that point.
    let cases = [
        (
            "before_insert",
            Operation::Insert,
            vec!["A:before_insert:Insert"],
        ),
        (
            "validate",
            Operation::Insert,
            vec![
                "A:before_insert:Insert",
                "B:before_insert:Insert",
                "A:validate:Insert",
            ],
        ),
        ("validate", Operation::Update, vec!["A:validate:Update"]),
        (
            "before_delete",
            Operation::Delete,
            vec!["A:before_delete:Delete"],
        ),
    ];
    for (hook, operation, expected_log) in cases {
        let (reg, log) = recording(Some((hook, operation)));
        let result = match operation {
            Operation::Insert => insert(
                &mut **client,
                &reg,
                "invoice",
                invoice("new", vec![item(5, "x")]),
                &Caller::system("ann"),
            )
            .await
            .map(|_| ()),
            Operation::Update => update(
                &mut **client,
                &reg,
                "invoice",
                &stored.name,
                1,
                &Changes {
                    values: BTreeMap::from([("title".to_owned(), text("changed"))]),
                    tables: BTreeMap::from([("items".to_owned(), vec![item(8, "y")])]),
                },
                &Caller::system("ann"),
            )
            .await
            .map(|_| ()),
            _ => {
                delete(
                    &mut **client,
                    &reg,
                    "invoice",
                    &stored.name,
                    1,
                    &Caller::system("ann"),
                )
                .await
            }
        };
        let error = result.unwrap_err();
        match &error {
            DbError::Hook {
                doctype,
                hook: failed,
                message,
            } => {
                assert_eq!((doctype.as_str(), *failed), ("invoice", hook));
                assert_eq!(message, "A says no");
            }
            other => panic!("{hook} {operation:?}: {other}"),
        }
        assert_eq!(taken(&log), expected_log, "{hook} {operation:?}");
        // Nothing changed: counts, version, values and row ids.
        assert_eq!(count(&db, "doc_invoice").await, 1);
        assert_eq!(count(&db, "doc_invoice_item").await, 1);
        assert_eq!(item_ids(&db).await, ids);
        let now = get(
            &mut **client,
            &plain,
            "invoice",
            &stored.name,
            &Caller::system("cli"),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(now, stored, "{hook} {operation:?}");
        assert_eq!(now.version, 1);
    }
}

#[tokio::test]
async fn a_hook_error_leaves_the_callers_transaction_usable() {
    let (db, _) = setup().await;
    let (reg, _) = recording(Some(("validate", Operation::Insert)));
    let (ok_reg, _) = recording(None);
    let mut client = db.pool.get().await.unwrap();
    let raw: &mut forgebase_db::Client = &mut client;

    let mut tx = raw.transaction().await.unwrap();
    insert(
        &mut tx,
        &ok_reg,
        "invoice",
        invoice("kept", vec![item(1, "a")]),
        &Caller::system("ann"),
    )
    .await
    .unwrap();
    let error = insert(
        &mut tx,
        &reg,
        "invoice",
        invoice("lost", vec![]),
        &Caller::system("ann"),
    )
    .await
    .unwrap_err();
    assert!(matches!(error, DbError::Hook { .. }), "{error}");
    // The transaction still holds the first insert and accepts more work.
    insert(
        &mut tx,
        &ok_reg,
        "invoice",
        invoice("more", vec![]),
        &Caller::system("ann"),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(count(&db, "doc_invoice").await, 2);
    assert_eq!(count(&db, "doc_invoice_item").await, 1);
}

#[tokio::test]
async fn update_and_delete_hook_errors_leave_the_callers_transaction_usable() {
    let (db, _) = setup().await;
    let (ok_reg, _) = recording(None);
    let (bad_update, _) = recording(Some(("validate", Operation::Update)));
    let (bad_delete, _) = recording(Some(("before_delete", Operation::Delete)));
    let mut client = db.pool.get().await.unwrap();
    let raw: &mut forgebase_db::Client = &mut client;

    let mut tx = raw.transaction().await.unwrap();
    let doc = insert(
        &mut tx,
        &ok_reg,
        "invoice",
        invoice("kept", vec![item(1, "a")]),
        &Caller::system("ann"),
    )
    .await
    .unwrap();
    let doc_name = doc.name.as_str();

    let error = update(
        &mut tx,
        &bad_update,
        "invoice",
        doc_name,
        1,
        &set("title", text("lost")),
        &Caller::system("ann"),
    )
    .await
    .unwrap_err();
    assert!(matches!(error, DbError::Hook { .. }), "{error}");
    let error = delete(
        &mut tx,
        &bad_delete,
        "invoice",
        doc_name,
        1,
        &Caller::system("ann"),
    )
    .await
    .unwrap_err();
    assert!(matches!(error, DbError::Hook { .. }), "{error}");

    // The transaction still holds the insert, unchanged, and accepts more work.
    let still = get(
        &mut tx,
        &ok_reg,
        "invoice",
        doc_name,
        &Caller::system("cli"),
    )
    .await
    .unwrap()
    .expect("kept");
    assert_eq!(
        (still.version, still.values["title"].clone()),
        (1, text("kept"))
    );
    let updated = update(
        &mut tx,
        &ok_reg,
        "invoice",
        doc_name,
        1,
        &set("title", text("changed")),
        &Caller::system("ann"),
    )
    .await
    .unwrap();
    assert_eq!(updated.version, 2);
    delete(
        &mut tx,
        &ok_reg,
        "invoice",
        doc_name,
        2,
        &Caller::system("ann"),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(count(&db, "doc_invoice").await, 0);
    assert_eq!(count(&db, "doc_invoice_item").await, 0);
}

#[tokio::test]
async fn an_update_in_progress_does_not_block_a_concurrent_link_to_the_document() {
    const NOTE: &str = r#"
name = "note"
label = "Note"
[[fields]]
name = "account"
type = "link"
target = "account"
"#;
    let db = TestDb::migrated().await;
    let reg = registry(&[ACCOUNT, NOTE]);
    db.sync(&reg).await.expect("sync");
    let account = {
        let mut client = db.pool.get().await.unwrap();
        let doc = Document {
            name: None,
            values: BTreeMap::from([("email".to_owned(), text("a@x.y"))]),
            tables: BTreeMap::new(),
        };
        insert(&mut **client, &reg, "account", doc, &Caller::system("ann"))
            .await
            .unwrap()
    };
    let mut first = db.pool.get().await.unwrap();
    let mut second = db.pool.get().await.unwrap();
    second
        .batch_execute("SET lock_timeout = '300ms'")
        .await
        .unwrap();

    // The first transaction updates the account and keeps its row lock (not committed).
    let raw: &mut forgebase_db::Client = &mut first;
    let mut tx = raw.transaction().await.unwrap();
    update(
        &mut tx,
        &reg,
        "account",
        &account.name,
        1,
        &set("points", Value::Int(5)),
        &Caller::system("ann"),
    )
    .await
    .unwrap();

    // A concurrent insert of a document that links to the account is not blocked: the lookup
    // takes FOR KEY SHARE, which FOR NO KEY UPDATE does not conflict with.
    let linked = Document {
        name: None,
        values: BTreeMap::from([("account".to_owned(), text("a@x.y"))]),
        tables: BTreeMap::new(),
    };
    tokio::time::timeout(
        Duration::from_secs(5),
        insert(&mut **second, &reg, "note", linked, &Caller::system("bob")),
    )
    .await
    .expect("the insert should not wait for the update")
    .expect("insert");
    assert_eq!(count(&db, "doc_note").await, 1);
    tx.commit().await.unwrap();
}

#[tokio::test]
async fn an_update_keeps_the_row_ids_of_a_table_with_identical_rows_and_replaces_changed_ones() {
    let (db, reg) = setup().await;
    let mut client = db.pool.get().await.unwrap();
    let rows = vec![item(1, "a"), item(2, "b")];
    let doc = insert(
        &mut **client,
        &reg,
        "invoice",
        invoice("t", rows.clone()),
        &Caller::system("ann"),
    )
    .await
    .unwrap();
    let ids = item_ids(&db).await;
    assert_eq!(ids.len(), 2);

    // The changes name the table with the rows it already has: same ids, version still bumped.
    let same = Changes {
        values: BTreeMap::from([("title".to_owned(), text("t2"))]),
        tables: BTreeMap::from([("items".to_owned(), rows)]),
    };
    let v2 = update(
        &mut **client,
        &reg,
        "invoice",
        &doc.name,
        1,
        &same,
        &Caller::system("ann"),
    )
    .await
    .unwrap();
    assert_eq!(v2.version, 2);
    assert_eq!(item_ids(&db).await, ids);

    // Changed rows replace the old ones: new ids.
    let changed = Changes {
        values: BTreeMap::new(),
        tables: BTreeMap::from([("items".to_owned(), vec![item(1, "a"), item(3, "c")])]),
    };
    let v3 = update(
        &mut **client,
        &reg,
        "invoice",
        &doc.name,
        2,
        &changed,
        &Caller::system("ann"),
    )
    .await
    .unwrap();
    assert_eq!(v3.tables["items"].len(), 2);
    let new_ids = item_ids(&db).await;
    assert_eq!(new_ids.len(), 2);
    assert!(new_ids.iter().all(|id| !ids.contains(id)));
}

#[tokio::test]
async fn not_found_and_conflict_are_reported_before_any_hook_runs() {
    let (db, _) = setup().await;
    let (reg, log) = recording(None);
    let mut client = db.pool.get().await.unwrap();
    let stored = insert(
        &mut **client,
        &reg,
        "invoice",
        invoice("a", vec![]),
        &Caller::system("ann"),
    )
    .await
    .unwrap();
    taken(&log);

    let error = update(
        &mut **client,
        &reg,
        "invoice",
        &stored.name,
        5,
        &set("note", text("n")),
        &Caller::system("ann"),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(
            error,
            DbError::Conflict {
                expected: 5,
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
        "nope",
        1,
        &set("note", text("n")),
        &Caller::system("ann"),
    )
    .await
    .unwrap_err();
    assert!(matches!(error, DbError::NotFound { .. }), "{error}");
    let error = delete(
        &mut **client,
        &reg,
        "invoice",
        &stored.name,
        5,
        &Caller::system("ann"),
    )
    .await
    .unwrap_err();
    assert!(matches!(error, DbError::Conflict { .. }), "{error}");
    let error = delete(
        &mut **client,
        &reg,
        "invoice",
        &stored.name,
        1,
        &Caller::system(""),
    )
    .await
    .unwrap_err();
    assert!(matches!(error, DbError::InvalidActor), "{error}");
    let error = delete(
        &mut **client,
        &reg,
        "invoice",
        "nope",
        1,
        &Caller::system("ann"),
    )
    .await
    .unwrap_err();
    assert!(matches!(error, DbError::NotFound { .. }), "{error}");
    assert!(taken(&log).is_empty(), "no hook was called");
}

#[tokio::test]
async fn update_validates_the_whole_document() {
    let (db, reg) = setup().await;
    let mut client = db.pool.get().await.unwrap();
    let stored = insert(
        &mut **client,
        &reg,
        "invoice",
        invoice("a", vec![]),
        &Caller::system("ann"),
    )
    .await
    .unwrap();
    // Another writer left a required field empty (the column is nullable).
    db.exec("UPDATE doc_invoice SET title = NULL").await;

    // The changes do not touch the title, but the document as a whole is checked.
    let error = update(
        &mut **client,
        &reg,
        "invoice",
        &stored.name,
        1,
        &set("note", text("n")),
        &Caller::system("ann"),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(&error, DbError::Validation(e) if e == &[ValidationError::Required { field: "title".into() }]),
        "{error}"
    );
    // Fixing it in the same update works.
    let fixed = update(
        &mut **client,
        &reg,
        "invoice",
        &stored.name,
        1,
        &set("title", text("fixed")),
        &Caller::system("ann"),
    )
    .await
    .unwrap();
    assert_eq!(titles(&fixed), (text("fixed"), Value::Null));
    assert_eq!(fixed.version, 2);
}

#[tokio::test]
async fn a_hook_cannot_rename_a_document() {
    struct Renamer;
    impl DocTypeHooks for Renamer {
        fn validate(&self, ctx: &HookContext<'_>, doc: &mut Document) -> Result<(), HookError> {
            if ctx.operation == Operation::Update {
                doc.values.insert("email".to_owned(), text("other"));
            }
            Ok(())
        }
    }
    let (db, _) = setup().await;
    let mut reg = registry(&[INVOICE, ITEM, ACCOUNT]);
    reg.add_hooks("account", Arc::new(Renamer)).unwrap();
    let mut client = db.pool.get().await.unwrap();
    let new = Document {
        name: None,
        values: BTreeMap::from([("email".to_owned(), text("old"))]),
        tables: BTreeMap::new(),
    };
    insert(&mut **client, &reg, "account", new, &Caller::system("ann"))
        .await
        .unwrap();
    let error = update(
        &mut **client,
        &reg,
        "account",
        "old",
        1,
        &set("points", Value::Int(1)),
        &Caller::system("ann"),
    )
    .await
    .unwrap_err();
    assert!(matches!(error, DbError::Naming(_)), "{error}");
    assert_eq!(count(&db, "doc_account").await, 1);
    let stored = get(
        &mut **client,
        &reg,
        "account",
        "old",
        &Caller::system("cli"),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!((stored.version, &stored.values["email"]), (1, &text("old")));
}

#[tokio::test]
async fn child_doctypes_have_no_hooks_and_other_doctypes_are_not_affected() {
    let (db, _) = setup().await;
    let log = Log::default();
    let mut reg = registry(&[INVOICE, ITEM, ACCOUNT]);
    let hooks = Recorder {
        id: "A",
        log: log.clone(),
        fail_at: None,
    };
    assert!(reg.add_hooks("invoice_item", Arc::new(hooks)).is_err());
    let hooks = Recorder {
        id: "A",
        log: log.clone(),
        fail_at: Some(("before_insert", Operation::Insert)),
    };
    reg.add_hooks("invoice", Arc::new(hooks)).unwrap();
    let mut client = db.pool.get().await.unwrap();
    // `account` has no hooks: the failing invoice hooks are not called for it.
    let new = Document {
        name: None,
        values: BTreeMap::from([("email".to_owned(), text("x"))]),
        tables: BTreeMap::new(),
    };
    insert(&mut **client, &reg, "account", new, &Caller::system("ann"))
        .await
        .unwrap();
    assert!(taken(&log).is_empty());
}
