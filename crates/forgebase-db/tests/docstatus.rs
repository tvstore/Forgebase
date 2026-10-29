//! docstatus, submit, cancel and amend (M2.4) against a real PostgreSQL (see `common`): the
//! transition matrix, the locks on a submitted document, the hook order and the rollbacks.

mod common;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use common::{TestDb, registry};
use forgebase_core::document::{Changes, DocStatus, Document, Row, ValidationError};
use forgebase_core::hooks::{DocTypeHooks, HookContext, HookError, Operation};
use forgebase_core::meta::Registry;
use forgebase_core::permission::Caller;
use forgebase_core::value::Value;
use forgebase_db::{DbError, StoredDocument, amend, cancel, delete, get, insert, submit, update};

const QUOTE: &str = r#"
name = "quote"
label = "Quote"
submittable = true
[[fields]]
name = "title"
type = "data"
required = true
[[fields]]
name = "total"
type = "int"
[[fields]]
name = "remarks"
type = "text"
allow_on_submit = true
[[fields]]
name = "lines"
type = "table"
target = "quote_line"
"#;

const LINE: &str = r#"
name = "quote_line"
label = "Quote Line"
is_child = true
[[fields]]
name = "qty"
type = "int"
required = true
[[fields]]
name = "note"
type = "data"
"#;

/// Not submittable.
const NOTE: &str = r#"
name = "note"
label = "Note"
[[fields]]
name = "body"
type = "text"
"#;

type Log = Arc<Mutex<Vec<String>>>;

fn text(s: &str) -> Value {
    Value::Text(s.to_owned())
}

fn line(qty: i64, note: &str) -> Row {
    BTreeMap::from([
        ("qty".to_owned(), Value::Int(qty)),
        ("note".to_owned(), text(note)),
    ])
}

fn quote(title: &str, lines: Vec<Row>) -> Document {
    Document {
        name: None,
        values: BTreeMap::from([("title".to_owned(), text(title))]),
        tables: BTreeMap::from([("lines".to_owned(), lines)]),
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
    fn before_submit(&self, ctx: &HookContext<'_>, _doc: &mut Document) -> Result<(), HookError> {
        self.record("before_submit", ctx)
    }
    fn before_cancel(&self, ctx: &HookContext<'_>, _doc: &Document) -> Result<(), HookError> {
        self.record("before_cancel", ctx)
    }
}

/// The quote DocTypes with two recorders (`A` registered first) and their log.
fn recording(fail_at: Option<(&'static str, Operation)>) -> (Registry, Log) {
    let log = Log::default();
    let mut reg = registry(&[QUOTE, LINE, NOTE]);
    for (id, fail) in [("A", fail_at), ("B", None)] {
        let hooks = Recorder {
            id,
            log: log.clone(),
            fail_at: fail,
        };
        reg.add_hooks("quote", Arc::new(hooks)).expect("hooks");
    }
    (reg, log)
}

async fn setup() -> (TestDb, Registry) {
    let db = TestDb::migrated().await;
    let reg = registry(&[QUOTE, LINE, NOTE]);
    db.sync(&reg).await.expect("sync");
    (db, reg)
}

async fn count(db: &TestDb, table: &str) -> i64 {
    db.rows(&format!("SELECT count(*)::text FROM {table}"))
        .await[0][0]
        .parse()
        .expect("count")
}

/// A fresh quote with two lines, brought to the given state.
async fn make(db: &TestDb, reg: &Registry, status: DocStatus) -> StoredDocument {
    let mut client = db.pool.get().await.unwrap();
    let c = &mut **client;
    let doc = quote("q", vec![line(1, "a"), line(2, "b")]);
    let mut stored = insert(c, reg, "quote", doc, &Caller::system("ann"))
        .await
        .unwrap();
    if status != DocStatus::Draft {
        stored = submit(
            c,
            reg,
            "quote",
            &stored.name,
            stored.version,
            &Caller::system("ann"),
        )
        .await
        .unwrap();
    }
    if status == DocStatus::Cancelled {
        stored = cancel(
            c,
            reg,
            "quote",
            &stored.name,
            stored.version,
            &Caller::system("ann"),
        )
        .await
        .unwrap();
    }
    assert_eq!(stored.docstatus, Some(status));
    stored
}

async fn reread(db: &TestDb, reg: &Registry, name: &str) -> Option<StoredDocument> {
    let mut client = db.pool.get().await.unwrap();
    get(&mut **client, reg, "quote", name, &Caller::system("cli"))
        .await
        .unwrap()
}

#[tokio::test]
async fn a_new_document_is_a_draft_and_users_cannot_set_docstatus_or_amended_from() {
    let (db, reg) = setup().await;
    let mut client = db.pool.get().await.unwrap();
    let c = &mut **client;
    let stored = insert(c, &reg, "quote", quote("a", vec![]), &Caller::system("ann"))
        .await
        .unwrap();
    assert_eq!(stored.docstatus, Some(DocStatus::Draft));
    assert_eq!(stored.amended_from, None);
    assert_eq!(stored.version, 1);

    for field in ["docstatus", "amended_from"] {
        let mut doc = quote("b", vec![]);
        doc.values.insert(field.to_owned(), Value::Int(1));
        let error = insert(c, &reg, "quote", doc, &Caller::system("ann"))
            .await
            .unwrap_err();
        assert!(
            matches!(&error, DbError::Validation(e) if e.contains(&ValidationError::UnknownField { field: field.into() })),
            "{field}: {error}"
        );
        let error = update(
            c,
            &reg,
            "quote",
            &stored.name,
            1,
            &set(field, Value::Int(1)),
            &Caller::system("ann"),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(&error, DbError::Validation(e) if e.contains(&ValidationError::UnknownField { field: field.into() })),
            "{field}: {error}"
        );
    }
    assert_eq!(count(&db, "doc_quote").await, 1);
    // A plain DocType has no docstatus at all.
    let note = insert(c, &reg, "note", Document::default(), &Caller::system("ann"))
        .await
        .unwrap();
    assert_eq!((note.docstatus, note.amended_from), (None, None));
}

#[tokio::test]
async fn submit_cancel_and_amend_on_a_plain_doctype_are_refused() {
    let (db, reg) = setup().await;
    let mut client = db.pool.get().await.unwrap();
    let c = &mut **client;
    let note = insert(c, &reg, "note", Document::default(), &Caller::system("ann"))
        .await
        .unwrap();
    for (operation, error) in [
        (
            "submit",
            submit(c, &reg, "note", &note.name, 1, &Caller::system("ann"))
                .await
                .unwrap_err(),
        ),
        (
            "cancel",
            cancel(c, &reg, "note", &note.name, 1, &Caller::system("ann"))
                .await
                .unwrap_err(),
        ),
        (
            "amend",
            amend(c, &reg, "note", &note.name, 1, &Caller::system("ann"))
                .await
                .unwrap_err(),
        ),
    ] {
        assert!(
            matches!(&error, DbError::NotSubmittable { doctype, operation: op } if doctype == "note" && *op == operation),
            "{operation}: {error}"
        );
        assert!(error.to_string().contains("not submittable"), "{error}");
    }
    // Not even a missing document is looked for first.
    assert!(matches!(
        submit(c, &reg, "note", "nope", 1, &Caller::system("ann")).await,
        Err(DbError::NotSubmittable { .. })
    ));
}

/// What each operation does to a document in each docstatus; `Ok` leaves the state changed as
/// the operation says, an error leaves everything as it was.
#[tokio::test]
async fn the_docstatus_transition_matrix() {
    use DocStatus::{Cancelled, Draft, Submitted};
    let (db, reg) = setup().await;
    // (operation, state, allowed)
    let matrix = [
        ("update", Draft, true),
        ("update", Submitted, true), // only an allow_on_submit field, see below
        ("update", Cancelled, false),
        ("submit", Draft, true),
        ("submit", Submitted, false),
        ("submit", Cancelled, false),
        ("cancel", Draft, false),
        ("cancel", Submitted, true),
        ("cancel", Cancelled, false),
        ("amend", Draft, false),
        ("amend", Submitted, false),
        ("amend", Cancelled, true),
        ("delete", Draft, true),
        ("delete", Submitted, false),
        ("delete", Cancelled, true),
    ];
    for (operation, state, allowed) in matrix {
        let before = make(&db, &reg, state).await;
        let (name, version) = (before.name.clone(), before.version);
        let mut client = db.pool.get().await.unwrap();
        let c = &mut **client;
        let result: Result<Option<StoredDocument>, DbError> = match operation {
            "update" => update(
                c,
                &reg,
                "quote",
                &name,
                version,
                &set("remarks", text("r")),
                &Caller::system("bob"),
            )
            .await
            .map(Some),
            "submit" => submit(c, &reg, "quote", &name, version, &Caller::system("bob"))
                .await
                .map(Some),
            "cancel" => cancel(c, &reg, "quote", &name, version, &Caller::system("bob"))
                .await
                .map(Some),
            "amend" => amend(c, &reg, "quote", &name, version, &Caller::system("bob"))
                .await
                .map(Some),
            _ => delete(c, &reg, "quote", &name, version, &Caller::system("bob"))
                .await
                .map(|()| None),
        };
        let case = format!("{operation} on a {state} document");
        match (allowed, result) {
            (true, Ok(after)) => {
                let expected = match operation {
                    "update" => Some(state),
                    "submit" => Some(Submitted),
                    "cancel" => Some(Cancelled),
                    "amend" => Some(Draft),
                    _ => None,
                };
                assert_eq!(after.as_ref().and_then(|a| a.docstatus), expected, "{case}");
            }
            (
                false,
                Err(DbError::InvalidDocStatus {
                    doctype,
                    name: n,
                    docstatus,
                    operation: op,
                }),
            ) => {
                assert_eq!(
                    (doctype.as_str(), &n, docstatus, op),
                    ("quote", &name, state, operation),
                    "{case}"
                );
                let now = reread(&db, &reg, &name).await.expect("still there");
                assert_eq!(
                    (now.docstatus, now.version),
                    (Some(state), version),
                    "{case}: unchanged"
                );
                assert_eq!(now.values, before.values, "{case}: unchanged");
            }
            (allowed, other) => panic!("{case}: allowed={allowed}, got {other:?}"),
        }
    }
    // Messages say what was refused and why.
    let submitted = make(&db, &reg, Submitted).await;
    let mut client = db.pool.get().await.unwrap();
    let error = delete(
        &mut **client,
        &reg,
        "quote",
        &submitted.name,
        submitted.version,
        &Caller::system("ann"),
    )
    .await
    .unwrap_err();
    assert_eq!(
        error.to_string(),
        format!("cannot delete quote `{}`: it is submitted", submitted.name)
    );
}

#[tokio::test]
async fn submit_and_cancel_bump_the_version_and_record_the_actor() {
    let (db, reg) = setup().await;
    let mut client = db.pool.get().await.unwrap();
    let c = &mut **client;
    let draft = insert(
        c,
        &reg,
        "quote",
        quote("a", vec![line(1, "x")]),
        &Caller::system("ann"),
    )
    .await
    .unwrap();
    let submitted = submit(c, &reg, "quote", &draft.name, 1, &Caller::system("bob"))
        .await
        .unwrap();
    assert_eq!(submitted.docstatus, Some(DocStatus::Submitted));
    assert_eq!(submitted.version, 2);
    assert_eq!(
        (submitted.owner.as_str(), submitted.modified_by.as_str()),
        ("ann", "bob")
    );
    assert!(submitted.modified_at >= draft.modified_at);
    assert_eq!(submitted.values, draft.values);
    assert_eq!(submitted.tables, draft.tables, "rows are untouched");
    // Row ids stay too: a submit with no hook changes does not replace rows.
    let cancelled = cancel(c, &reg, "quote", &draft.name, 2, &Caller::system("cy"))
        .await
        .unwrap();
    assert_eq!(cancelled.docstatus, Some(DocStatus::Cancelled));
    assert_eq!(
        (cancelled.version, cancelled.modified_by.as_str()),
        (3, "cy")
    );
    assert_eq!(cancelled.tables, draft.tables);
}

#[tokio::test]
async fn stale_versions_and_unknown_names_are_reported_first() {
    let (db, reg) = setup().await;
    let (reg_hooked, log) = recording(None);
    let _ = reg;
    let mut client = db.pool.get().await.unwrap();
    let c = &mut **client;
    let draft = insert(
        c,
        &reg_hooked,
        "quote",
        quote("a", vec![]),
        &Caller::system("ann"),
    )
    .await
    .unwrap();
    taken(&log);
    let error = submit(
        c,
        &reg_hooked,
        "quote",
        &draft.name,
        7,
        &Caller::system("ann"),
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
    assert!(matches!(
        cancel(c, &reg_hooked, "quote", "nope", 1, &Caller::system("ann")).await,
        Err(DbError::NotFound { .. })
    ));
    assert!(matches!(
        amend(
            c,
            &reg_hooked,
            "quote",
            &draft.name,
            9,
            &Caller::system("ann")
        )
        .await,
        Err(DbError::Conflict { .. })
    ));
    // Not found and conflict come before any hook runs.
    assert!(taken(&log).is_empty());
    assert!(matches!(
        submit(c, &reg_hooked, "quote", &draft.name, 1, &Caller::system("")).await,
        Err(DbError::InvalidActor)
    ));
}

#[tokio::test]
async fn a_submitted_document_only_changes_allow_on_submit_fields() {
    let (db, reg) = setup().await;
    let submitted = make(&db, &reg, DocStatus::Submitted).await;
    let (name, version) = (submitted.name.clone(), submitted.version);
    let mut client = db.pool.get().await.unwrap();
    let c = &mut **client;

    // A locked field, a table and a mix are refused, and nothing changes.
    let mut mixed = set("remarks", text("sneaky"));
    mixed.values.insert("title".to_owned(), text("changed"));
    let table_only = Changes {
        values: BTreeMap::new(),
        tables: BTreeMap::from([("lines".to_owned(), vec![line(9, "z")])]),
    };
    for (changes, field) in [
        (set("title", text("changed")), "title"),
        (set("total", Value::Int(5)), "total"),
        (mixed, "title"),
        (table_only, "lines"),
    ] {
        let error = update(
            c,
            &reg,
            "quote",
            &name,
            version,
            &changes,
            &Caller::system("bob"),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(&error, DbError::FieldLocked { doctype, name: n, field: f } if doctype == "quote" && *n == name && f == field),
            "{field}: {error}"
        );
    }
    let now = reread(&db, &reg, &name).await.unwrap();
    assert_eq!(now, submitted, "a refused update changed nothing");

    // An allow_on_submit field may change; the version moves, the status stays.
    let updated = update(
        c,
        &reg,
        "quote",
        &name,
        version,
        &set("remarks", text("ok")),
        &Caller::system("bob"),
    )
    .await
    .unwrap();
    assert_eq!(updated.docstatus, Some(DocStatus::Submitted));
    assert_eq!(updated.version, version + 1);
    assert_eq!(updated.values["remarks"], text("ok"));
    assert_eq!(updated.values["title"], submitted.values["title"]);
    assert_eq!(updated.tables, submitted.tables);
    assert_eq!(updated.modified_by, "bob");

    // Repeating a locked field's stored value is no change, so it is allowed with a real one.
    let mut same = set("title", text("q"));
    same.values.insert("remarks".to_owned(), text("again"));
    same.tables
        .insert("lines".to_owned(), submitted.tables["lines"].clone());
    let updated = update(
        c,
        &reg,
        "quote",
        &name,
        version + 1,
        &same,
        &Caller::system("bob"),
    )
    .await
    .unwrap();
    assert_eq!(updated.values["remarks"], text("again"));
    assert_eq!(updated.tables, submitted.tables);

    // Clearing an allow_on_submit field works; validation still applies to the whole document.
    let updated = update(
        c,
        &reg,
        "quote",
        &name,
        version + 2,
        &set("remarks", Value::Null),
        &Caller::system("bob"),
    )
    .await
    .unwrap();
    assert_eq!(updated.values["remarks"], Value::Null);
    let error = update(
        c,
        &reg,
        "quote",
        &name,
        version + 3,
        &set("remarks", Value::Int(1)),
        &Caller::system("bob"),
    )
    .await
    .unwrap_err();
    assert!(matches!(error, DbError::Validation(_)), "{error}");
}

/// Counts the updates in `total` (locked once submitted) in `validate`, on update only.
struct Sneaky;

impl DocTypeHooks for Sneaky {
    fn validate(&self, ctx: &HookContext<'_>, doc: &mut Document) -> Result<(), HookError> {
        if ctx.operation == Operation::Update {
            let n = match doc.values.get("total") {
                Some(Value::Int(n)) => *n,
                _ => 0,
            };
            doc.values.insert("total".to_owned(), Value::Int(n + 1));
        }
        Ok(())
    }
}

#[tokio::test]
async fn a_hook_cannot_change_a_locked_field_of_a_submitted_document() {
    let (db, _) = setup().await;
    let plain = registry(&[QUOTE, LINE, NOTE]);
    let mut hooked = registry(&[QUOTE, LINE, NOTE]);
    hooked.add_hooks("quote", Arc::new(Sneaky)).unwrap();
    let draft = make(&db, &plain, DocStatus::Draft).await;
    let mut client = db.pool.get().await.unwrap();
    let c = &mut **client;

    // On a draft the hook may change the total ...
    let updated = update(
        c,
        &hooked,
        "quote",
        &draft.name,
        1,
        &set("remarks", text("r")),
        &Caller::system("ann"),
    )
    .await
    .unwrap();
    assert_eq!(updated.values["total"], Value::Int(1));
    // ... on a submitted document it may not (the total is locked).
    let submitted = submit(c, &plain, "quote", &draft.name, 2, &Caller::system("ann"))
        .await
        .unwrap();
    let error = update(
        c,
        &hooked,
        "quote",
        &draft.name,
        3,
        &set("remarks", text("r2")),
        &Caller::system("ann"),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(&error, DbError::FieldLocked { field, .. } if field == "total"),
        "{error}"
    );
    // The hook's change was refused: the document is as it was after the submit.
    assert_eq!(reread(&db, &plain, &draft.name).await.unwrap(), submitted);
}

#[tokio::test]
async fn hooks_run_in_the_documented_order_for_submit_cancel_amend_and_delete() {
    let (db, _) = setup().await;
    let (reg, log) = recording(None);
    let mut client = db.pool.get().await.unwrap();
    let c = &mut **client;
    let draft = insert(c, &reg, "quote", quote("a", vec![]), &Caller::system("ann"))
        .await
        .unwrap();
    taken(&log);

    submit(c, &reg, "quote", &draft.name, 1, &Caller::system("ann"))
        .await
        .unwrap();
    assert_eq!(
        taken(&log),
        [
            "A:validate:Submit",
            "B:validate:Submit",
            "A:before_submit:Submit",
            "B:before_submit:Submit"
        ]
    );
    cancel(c, &reg, "quote", &draft.name, 2, &Caller::system("ann"))
        .await
        .unwrap();
    assert_eq!(
        taken(&log),
        ["A:before_cancel:Cancel", "B:before_cancel:Cancel"]
    );

    let amended = amend(c, &reg, "quote", &draft.name, 3, &Caller::system("ann"))
        .await
        .unwrap();
    // The insert pipeline, told apart by the operation.
    assert_eq!(
        taken(&log),
        [
            "A:before_insert:Amend",
            "B:before_insert:Amend",
            "A:validate:Amend",
            "B:validate:Amend"
        ]
    );
    delete(c, &reg, "quote", &amended.name, 1, &Caller::system("ann"))
        .await
        .unwrap();
    assert_eq!(
        taken(&log),
        ["A:before_delete:Delete", "B:before_delete:Delete"]
    );
    // A refused transition runs no hook.
    let error = cancel(c, &reg, "quote", &draft.name, 3, &Caller::system("ann"))
        .await
        .unwrap_err();
    assert!(matches!(error, DbError::InvalidDocStatus { .. }));
    assert!(taken(&log).is_empty());
}

/// Sets `total` and changes a row in `before_submit`.
struct Finalize;

impl DocTypeHooks for Finalize {
    fn before_submit(&self, _ctx: &HookContext<'_>, doc: &mut Document) -> Result<(), HookError> {
        let qty: i64 = doc.tables.get("lines").map_or(0, |rows| {
            rows.iter()
                .filter_map(|r| match r["qty"] {
                    Value::Int(n) => Some(n),
                    _ => None,
                })
                .sum()
        });
        doc.values.insert("total".to_owned(), Value::Int(qty));
        if let Some(rows) = doc.tables.get_mut("lines") {
            rows[0].insert("note".to_owned(), text("finalized"));
        }
        Ok(())
    }
}

#[tokio::test]
async fn changes_a_submit_hook_makes_are_persisted_with_the_docstatus() {
    let (db, _) = setup().await;
    let mut reg = registry(&[QUOTE, LINE, NOTE]);
    reg.add_hooks("quote", Arc::new(Finalize)).unwrap();
    let mut client = db.pool.get().await.unwrap();
    let c = &mut **client;
    let doc = quote("a", vec![line(2, "x"), line(3, "y")]);
    let draft = insert(c, &reg, "quote", doc, &Caller::system("ann"))
        .await
        .unwrap();
    assert_eq!(draft.values["total"], Value::Null);

    let submitted = submit(c, &reg, "quote", &draft.name, 1, &Caller::system("ann"))
        .await
        .unwrap();
    assert_eq!(submitted.values["total"], Value::Int(5));
    assert_eq!(submitted.tables["lines"][0]["note"], text("finalized"));
    assert_eq!(submitted.tables["lines"][1]["note"], text("y"));
    assert_eq!(submitted.version, 2);
    // What was returned is what is stored.
    assert_eq!(reread(&db, &reg, &draft.name).await.unwrap(), submitted);
}

/// Makes the document invalid in `before_submit`.
struct Break;

impl DocTypeHooks for Break {
    fn before_submit(&self, _ctx: &HookContext<'_>, doc: &mut Document) -> Result<(), HookError> {
        doc.values.remove("title");
        Ok(())
    }
}

#[tokio::test]
async fn submit_validates_the_result_of_the_hooks_and_changes_nothing_when_invalid() {
    let (db, _) = setup().await;
    let mut reg = registry(&[QUOTE, LINE, NOTE]);
    reg.add_hooks("quote", Arc::new(Break)).unwrap();
    let draft = make(&db, &reg, DocStatus::Draft).await;
    let mut client = db.pool.get().await.unwrap();
    let error = submit(
        &mut **client,
        &reg,
        "quote",
        &draft.name,
        1,
        &Caller::system("ann"),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(&error, DbError::Validation(e) if e == &[ValidationError::Required { field: "title".into() }]),
        "{error}"
    );
    assert_eq!(reread(&db, &reg, &draft.name).await.unwrap(), draft);
}

#[tokio::test]
async fn a_hook_failure_at_each_new_point_leaves_nothing_changed() {
    use Operation::{Amend, Cancel, Submit};
    let (db, _) = setup().await;
    // (failing hook, operation it fails for, the state the document is in when it is called)
    let cases = [
        ("validate", Submit, DocStatus::Draft),
        ("before_submit", Submit, DocStatus::Draft),
        ("before_cancel", Cancel, DocStatus::Submitted),
        ("before_insert", Amend, DocStatus::Cancelled),
        ("validate", Amend, DocStatus::Cancelled),
    ];
    for (hook, operation, state) in cases {
        let plain = registry(&[QUOTE, LINE, NOTE]);
        let doc = make(&db, &plain, state).await;
        let rows_before = count(&db, "doc_quote_line").await;
        let quotes_before = count(&db, "doc_quote").await;

        let (reg, log) = recording(Some((hook, operation)));
        let mut client = db.pool.get().await.unwrap();
        let c = &mut **client;
        let error = match operation {
            Submit => {
                submit(
                    c,
                    &reg,
                    "quote",
                    &doc.name,
                    doc.version,
                    &Caller::system("bob"),
                )
                .await
            }
            Cancel => {
                cancel(
                    c,
                    &reg,
                    "quote",
                    &doc.name,
                    doc.version,
                    &Caller::system("bob"),
                )
                .await
            }
            _ => {
                amend(
                    c,
                    &reg,
                    "quote",
                    &doc.name,
                    doc.version,
                    &Caller::system("bob"),
                )
                .await
            }
        }
        .unwrap_err();
        let case = format!("{hook} for {operation:?}");
        assert!(
            matches!(&error, DbError::Hook { doctype, hook: h, message } if doctype == "quote" && *h == hook && message == "A says no"),
            "{case}: {error}"
        );
        // The failing hook is the first one, so the second never ran.
        assert!(
            taken(&log).last().is_some_and(|l| l.starts_with("A:")),
            "{case}"
        );
        assert_eq!(reread(&db, &plain, &doc.name).await.unwrap(), doc, "{case}");
        assert_eq!(
            count(&db, "doc_quote").await,
            quotes_before,
            "{case}: no new document"
        );
        assert_eq!(
            count(&db, "doc_quote_line").await,
            rows_before,
            "{case}: no new rows"
        );
    }
}

#[tokio::test]
async fn an_operation_in_a_caller_transaction_rolls_back_with_it() {
    let (db, reg) = setup().await;
    let draft = make(&db, &reg, DocStatus::Draft).await;
    let mut client = db.pool.get().await.unwrap();
    let raw: &mut forgebase_db::Client = &mut client;
    {
        let mut tx = raw.transaction().await.unwrap();
        let submitted = submit(
            &mut tx,
            &reg,
            "quote",
            &draft.name,
            1,
            &Caller::system("ann"),
        )
        .await
        .unwrap();
        assert_eq!(submitted.docstatus, Some(DocStatus::Submitted));
        // A failed call inside the transaction leaves it usable (it ran in a savepoint).
        assert!(matches!(
            submit(
                &mut tx,
                &reg,
                "quote",
                &draft.name,
                2,
                &Caller::system("ann")
            )
            .await,
            Err(DbError::InvalidDocStatus { .. })
        ));
        cancel(
            &mut tx,
            &reg,
            "quote",
            &draft.name,
            2,
            &Caller::system("ann"),
        )
        .await
        .unwrap();
        tx.rollback().await.unwrap();
    }
    assert_eq!(reread(&db, &reg, &draft.name).await.unwrap(), draft);
}

#[tokio::test]
async fn amending_copies_the_document_into_a_new_draft_named_after_the_original() {
    let (db, reg) = setup().await;
    let doc = quote("a", vec![line(1, "x"), line(2, "y")]);
    let mut client = db.pool.get().await.unwrap();
    let c = &mut **client;
    let mut first = insert(c, &reg, "quote", doc, &Caller::system("ann"))
        .await
        .unwrap();
    first = update(
        c,
        &reg,
        "quote",
        &first.name,
        1,
        &set("total", Value::Int(3)),
        &Caller::system("ann"),
    )
    .await
    .unwrap();
    first = submit(c, &reg, "quote", &first.name, 2, &Caller::system("ann"))
        .await
        .unwrap();
    first = update(
        c,
        &reg,
        "quote",
        &first.name,
        3,
        &set("remarks", text("r")),
        &Caller::system("ann"),
    )
    .await
    .unwrap();
    first = cancel(c, &reg, "quote", &first.name, 4, &Caller::system("ann"))
        .await
        .unwrap();

    let second = amend(
        c,
        &reg,
        "quote",
        &first.name,
        first.version,
        &Caller::system("bob"),
    )
    .await
    .unwrap();
    assert_eq!(second.name, format!("{}-1", first.name));
    assert_eq!(second.amended_from.as_deref(), Some(first.name.as_str()));
    assert_eq!(second.docstatus, Some(DocStatus::Draft));
    assert_eq!(
        (
            second.version,
            second.owner.as_str(),
            second.modified_by.as_str()
        ),
        (1, "bob", "bob")
    );
    assert_ne!(second.id, first.id);
    // Values and rows are copies.
    assert_eq!(second.values, first.values);
    assert_eq!(second.values["total"], Value::Int(3));
    assert_eq!(second.tables, first.tables);
    // The cancelled document is untouched (same version, still cancelled).
    assert_eq!(reread(&db, &reg, &first.name).await.unwrap(), first);
    assert_eq!(count(&db, "doc_quote_line").await, 4);

    // The chain: X -> X-1 -> X-2 -> X-3, the root name stays.
    let second = submit(c, &reg, "quote", &second.name, 1, &Caller::system("bob"))
        .await
        .unwrap();
    let second = cancel(c, &reg, "quote", &second.name, 2, &Caller::system("bob"))
        .await
        .unwrap();
    let third = amend(c, &reg, "quote", &second.name, 3, &Caller::system("bob"))
        .await
        .unwrap();
    assert_eq!(third.name, format!("{}-2", first.name));
    assert_eq!(third.amended_from.as_deref(), Some(second.name.as_str()));
    let third = submit(c, &reg, "quote", &third.name, 1, &Caller::system("bob"))
        .await
        .unwrap();
    let third = cancel(c, &reg, "quote", &third.name, 2, &Caller::system("bob"))
        .await
        .unwrap();
    let fourth = amend(c, &reg, "quote", &third.name, 3, &Caller::system("bob"))
        .await
        .unwrap();
    assert_eq!(fourth.name, format!("{}-3", first.name));
    assert_eq!(fourth.amended_from.as_deref(), Some(third.name.as_str()));
}

#[tokio::test]
async fn a_cancelled_document_can_only_be_amended_once() {
    let (db, reg) = setup().await;
    let cancelled = make(&db, &reg, DocStatus::Cancelled).await;
    let mut client = db.pool.get().await.unwrap();
    let c = &mut **client;
    let first = amend(
        c,
        &reg,
        "quote",
        &cancelled.name,
        cancelled.version,
        &Caller::system("ann"),
    )
    .await
    .unwrap();
    let error = amend(
        c,
        &reg,
        "quote",
        &cancelled.name,
        cancelled.version,
        &Caller::system("ann"),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(&error, DbError::AlreadyAmended { doctype, name, amended_by }
            if doctype == "quote" && *name == cancelled.name && *amended_by == first.name),
        "{error}"
    );
    assert_eq!(
        error.to_string(),
        format!(
            "quote `{}` was already amended by `{}`",
            cancelled.name, first.name
        )
    );
    assert_eq!(count(&db, "doc_quote").await, 2);

    // Once the amendment is deleted the document can be amended again.
    delete(c, &reg, "quote", &first.name, 1, &Caller::system("ann"))
        .await
        .unwrap();
    let again = amend(
        c,
        &reg,
        "quote",
        &cancelled.name,
        cancelled.version,
        &Caller::system("ann"),
    )
    .await
    .unwrap();
    assert_eq!(again.name, first.name);
}

#[tokio::test]
async fn racing_amendments_of_one_document_succeed_exactly_once() {
    let (db, reg) = setup().await;
    let cancelled = make(&db, &reg, DocStatus::Cancelled).await;
    let (mut a, mut b) = (db.pool.get().await.unwrap(), db.pool.get().await.unwrap());
    let (caller_a, caller_b) = (Caller::system("a"), Caller::system("b"));
    let (ra, rb) = tokio::join!(
        amend(
            &mut **a,
            &reg,
            "quote",
            &cancelled.name,
            cancelled.version,
            &caller_a
        ),
        amend(
            &mut **b,
            &reg,
            "quote",
            &cancelled.name,
            cancelled.version,
            &caller_b
        ),
    );
    let outcomes = [ra, rb];
    assert_eq!(
        outcomes.iter().filter(|r| r.is_ok()).count(),
        1,
        "{outcomes:?}"
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|r| matches!(r, Err(DbError::AlreadyAmended { .. })))
            .count(),
        1,
        "{outcomes:?}"
    );
    assert_eq!(count(&db, "doc_quote").await, 2);
}

#[tokio::test]
async fn racing_submits_with_the_same_version_succeed_exactly_once() {
    let (db, reg) = setup().await;
    let draft = make(&db, &reg, DocStatus::Draft).await;
    let (mut a, mut b) = (db.pool.get().await.unwrap(), db.pool.get().await.unwrap());
    let (caller_a, caller_b) = (Caller::system("a"), Caller::system("b"));
    let (ra, rb) = tokio::join!(
        submit(&mut **a, &reg, "quote", &draft.name, 1, &caller_a),
        submit(&mut **b, &reg, "quote", &draft.name, 1, &caller_b),
    );
    let outcomes = [ra, rb];
    assert_eq!(
        outcomes.iter().filter(|r| r.is_ok()).count(),
        1,
        "{outcomes:?}"
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|r| matches!(r, Err(DbError::Conflict { current: 2, .. })))
            .count(),
        1,
        "{outcomes:?}"
    );
    assert_eq!(reread(&db, &reg, &draft.name).await.unwrap().version, 2);
}

#[tokio::test]
async fn delete_rules_by_docstatus() {
    let (db, reg) = setup().await;
    let mut client = db.pool.get().await.unwrap();
    let c = &mut **client;
    // A draft and a cancelled document can be deleted, with their rows; a submitted one cannot.
    let draft = make(&db, &reg, DocStatus::Draft).await;
    delete(
        c,
        &reg,
        "quote",
        &draft.name,
        draft.version,
        &Caller::system("ann"),
    )
    .await
    .unwrap();
    let cancelled = make(&db, &reg, DocStatus::Cancelled).await;
    delete(
        c,
        &reg,
        "quote",
        &cancelled.name,
        cancelled.version,
        &Caller::system("ann"),
    )
    .await
    .unwrap();
    assert_eq!(count(&db, "doc_quote").await, 0);
    assert_eq!(count(&db, "doc_quote_line").await, 0);

    let submitted = make(&db, &reg, DocStatus::Submitted).await;
    let error = delete(
        c,
        &reg,
        "quote",
        &submitted.name,
        submitted.version,
        &Caller::system("ann"),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(
            error,
            DbError::InvalidDocStatus {
                docstatus: DocStatus::Submitted,
                operation: "delete",
                ..
            }
        ),
        "{error}"
    );
    assert_eq!(count(&db, "doc_quote").await, 1);
    assert_eq!(count(&db, "doc_quote_line").await, 2);
    // Cancel it and it can go.
    let cancelled = cancel(
        c,
        &reg,
        "quote",
        &submitted.name,
        submitted.version,
        &Caller::system("ann"),
    )
    .await
    .unwrap();
    delete(
        c,
        &reg,
        "quote",
        &cancelled.name,
        cancelled.version,
        &Caller::system("ann"),
    )
    .await
    .unwrap();
    assert_eq!(count(&db, "doc_quote").await, 0);
}

#[tokio::test]
async fn amended_documents_see_the_hooks_context_and_the_new_name() {
    /// Keeps what `validate` and `before_insert` saw.
    struct Spy(Mutex<Vec<(Operation, Option<String>, Value)>>);
    struct Share(Arc<Spy>);
    impl DocTypeHooks for Share {
        fn before_insert(
            &self,
            ctx: &HookContext<'_>,
            doc: &mut Document,
        ) -> Result<(), HookError> {
            self.0.0.lock().unwrap().push((
                ctx.operation,
                doc.name.clone(),
                doc.values["title"].clone(),
            ));
            Ok(())
        }
    }
    let (db, plain) = setup().await;
    let cancelled = make(&db, &plain, DocStatus::Cancelled).await;
    let spy = Arc::new(Spy(Mutex::default()));
    let mut reg = registry(&[QUOTE, LINE, NOTE]);
    reg.add_hooks("quote", Arc::new(Share(spy.clone())))
        .unwrap();
    let mut client = db.pool.get().await.unwrap();
    let amended = amend(
        &mut **client,
        &reg,
        "quote",
        &cancelled.name,
        cancelled.version,
        &Caller::system("ann"),
    )
    .await
    .unwrap();
    let seen = spy.0.lock().unwrap();
    assert_eq!(
        *seen,
        [(Operation::Amend, Some(amended.name.clone()), text("q"))]
    );
}

/// A hook that renames the document it is amending.
struct Rename;

impl DocTypeHooks for Rename {
    fn before_insert(&self, ctx: &HookContext<'_>, doc: &mut Document) -> Result<(), HookError> {
        if ctx.operation == Operation::Amend {
            doc.name = Some("other".to_owned());
        }
        Ok(())
    }
}

#[tokio::test]
async fn a_hook_cannot_rename_an_amendment() {
    let (db, plain) = setup().await;
    let cancelled = make(&db, &plain, DocStatus::Cancelled).await;
    let mut reg = registry(&[QUOTE, LINE, NOTE]);
    reg.add_hooks("quote", Arc::new(Rename)).unwrap();
    let mut client = db.pool.get().await.unwrap();
    let error = amend(
        &mut **client,
        &reg,
        "quote",
        &cancelled.name,
        cancelled.version,
        &Caller::system("ann"),
    )
    .await
    .unwrap_err();
    assert!(matches!(error, DbError::Naming(_)), "{error}");
    assert_eq!(count(&db, "doc_quote").await, 1);
}

const TICKET: &str = r#"
name = "ticket"
label = "Ticket"
submittable = true
[[fields]]
name = "code"
type = "data"
unique = true
"#;

/// A hook that gives an amendment a new value for the unique `code` field.
struct NewCode;

impl DocTypeHooks for NewCode {
    fn before_insert(&self, ctx: &HookContext<'_>, doc: &mut Document) -> Result<(), HookError> {
        if ctx.operation == Operation::Amend {
            doc.values.insert("code".to_owned(), text("T-2"));
        }
        Ok(())
    }
}

#[tokio::test]
async fn amending_a_unique_field_needs_a_hook_that_changes_the_value() {
    let db = TestDb::migrated().await;
    let plain = registry(&[TICKET]);
    db.sync(&plain).await.expect("sync");
    let mut client = db.pool.get().await.unwrap();
    let c = &mut **client;
    let doc = Document {
        name: None,
        values: BTreeMap::from([("code".to_owned(), text("T-1"))]),
        tables: BTreeMap::new(),
    };
    let stored = insert(c, &plain, "ticket", doc, &Caller::system("ann"))
        .await
        .unwrap();
    let stored = submit(
        c,
        &plain,
        "ticket",
        &stored.name,
        stored.version,
        &Caller::system("ann"),
    )
    .await
    .unwrap();
    let cancelled = cancel(
        c,
        &plain,
        "ticket",
        &stored.name,
        stored.version,
        &Caller::system("ann"),
    )
    .await
    .unwrap();

    // The copy repeats `code`, which the cancelled original still holds.
    let error = amend(
        c,
        &plain,
        "ticket",
        &cancelled.name,
        cancelled.version,
        &Caller::system("ann"),
    )
    .await
    .unwrap_err();
    assert!(matches!(error, DbError::UniqueViolation { .. }), "{error}");
    assert_eq!(count(&db, "doc_ticket").await, 1);

    // A hook that checks `Operation::Amend` and changes the value makes it work.
    let mut hooked = registry(&[TICKET]);
    hooked.add_hooks("ticket", Arc::new(NewCode)).unwrap();
    let amended = amend(
        c,
        &hooked,
        "ticket",
        &cancelled.name,
        cancelled.version,
        &Caller::system("ann"),
    )
    .await
    .unwrap();
    assert_eq!(amended.values["code"], text("T-2"));
    assert_eq!(
        amended.amended_from.as_deref(),
        Some(cancelled.name.as_str())
    );
    assert_eq!(count(&db, "doc_ticket").await, 2);
}
