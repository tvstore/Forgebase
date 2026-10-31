//! Import and export (M9.3, ADR 0021) against a real PostgreSQL: the rights, the snapshot and the
//! pages of an export, and the all-or-nothing import with savepoints, its dry run, upsert and
//! per-row errors.

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;

use common::{TestDb, registry};
use forgebase_core::document::{Document, Row};
use forgebase_core::hooks::{DocTypeHooks, HookContext, HookError};
use forgebase_core::meta::Registry;
use forgebase_core::permission::Caller;
use forgebase_core::value::Value;
use forgebase_db::webhooks::{self, DELIVER_JOB, NewWebhook};
use forgebase_db::{
    DbError, Export, ExportQuery, Filter, FilterOp, ImportMode, ImportRow, MAX_REPORTED_ERRORS,
    PAGE_ROWS, Sort, StoredDocument, begin_export, get, import, insert,
};
use serde_json::json;

const PARCEL: &str = r#"
name = "parcel"
label = "Parcel"
[[fields]]
name = "title"
type = "data"
required = true
[[fields]]
name = "qty"
type = "int"
[[fields]]
name = "note"
type = "text"
[[fields]]
name = "lines"
type = "table"
target = "parcel_line"

[[permissions]]
role = "Reader"
read = true
[[permissions]]
role = "Plain Reader"
read = true
[[permissions]]
role = "Exporter"
export = true
[[permissions]]
role = "Reader Exporter"
read = true
export = true
[[permissions]]
role = "Own Reader Exporter"
read = true
export = true
if_owner = true
[[permissions]]
role = "Reader"
export = true
if_owner = true
[[permissions]]
role = "Importer"
import = true
create = true
[[permissions]]
role = "Import Only"
import = true
[[permissions]]
role = "Create Only"
create = true
[[permissions]]
role = "Upserter"
import = true
create = true
write = true
read = true
"#;

const PARCEL_LINE: &str = r#"
name = "parcel_line"
label = "Parcel Line"
is_child = true
[[fields]]
name = "item"
type = "data"
"#;

/// Named by its `code`.
const BADGE: &str = r#"
name = "badge"
label = "Badge"
naming = { field = "code" }
[[fields]]
name = "code"
type = "data"
required = true
unique = true
[[fields]]
name = "level"
type = "int"
[[permissions]]
role = "Upserter"
import = true
create = true
write = true
read = true
"#;

/// Sets the note of a parcel titled `hook-me`; refuses `reject`; enqueues an audit job.
struct Rules;

impl DocTypeHooks for Rules {
    fn validate(&self, ctx: &HookContext<'_>, doc: &mut Document) -> Result<(), HookError> {
        match doc.values.get("title") {
            Some(Value::Text(t)) if t == "reject" => return Err(HookError::new("rejected")),
            Some(Value::Text(t)) if t == "hook-me" => {
                doc.values.insert("note".into(), text("hooked"));
            }
            _ => {}
        }
        ctx.enqueue(
            "app.audit",
            json!({"title": doc.values.contains_key("title")}),
        )
    }
}

fn world() -> Registry {
    let mut registry = registry(&[PARCEL, PARCEL_LINE, BADGE]);
    registry
        .add_hooks("parcel", Arc::new(Rules))
        .expect("hooks");
    assert!(registry.add_job("app.audit"));
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

fn user(roles: &[&str]) -> Caller {
    Caller::user("u", roles.iter().map(|r| (*r).to_owned()).collect())
}

fn other(roles: &[&str]) -> Caller {
    Caller::user(
        "someone-else",
        roles.iter().map(|r| (*r).to_owned()).collect(),
    )
}

fn row(n: usize, title: &str) -> ImportRow {
    ImportRow {
        row: n,
        line: Some(n as u64 + 1),
        values: BTreeMap::from([("title".to_owned(), text(title))]),
        ..ImportRow::default()
    }
}

fn named(n: usize, name: &str, values: &[(&str, Value)]) -> ImportRow {
    ImportRow {
        row: n,
        name: Some(name.to_owned()),
        values: values
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.clone()))
            .collect(),
        ..ImportRow::default()
    }
}

async fn add_parcel(db: &TestDb, registry: &Registry, by: &Caller, title: &str) -> StoredDocument {
    let mut client = db.pool.get().await.unwrap();
    let doc = Document {
        name: None,
        values: BTreeMap::from([("title".to_owned(), text(title))]),
        tables: BTreeMap::from([(
            "lines".to_owned(),
            vec![
                Row::from([("item".to_owned(), text(&format!("{title}-a")))]),
                Row::from([("item".to_owned(), text(&format!("{title}-b")))]),
            ],
        )]),
    };
    insert(&mut **client, registry, "parcel", doc, by)
        .await
        .unwrap()
}

/// Every document of an export, page by page, with the number of pages.
async fn export_all(
    db: &TestDb,
    registry: &Registry,
    caller: &Caller,
    filters: &[Filter],
    sort: &[Sort],
    limit: u64,
) -> Result<(Vec<StoredDocument>, usize, u64), DbError> {
    let mut client = db.pool.get().await.unwrap();
    let tx = begin_export(&mut client).await?;
    let query = ExportQuery {
        doctype: "parcel",
        filters,
        sort,
        limit,
        with_tables: true,
    };
    let mut export = Export::start(&tx, registry, caller, &query).await?;
    let count = export.count();
    let mut all = Vec::new();
    let mut pages = 0;
    while let Some(page) = export.next_page(&tx, registry).await? {
        pages += 1;
        all.extend(page);
    }
    Ok((all, pages, count))
}

fn titles(docs: &[StoredDocument]) -> Vec<String> {
    docs.iter()
        .map(|d| match &d.values["title"] {
            Value::Text(t) => t.clone(),
            other => panic!("{other:?}"),
        })
        .collect()
}

async fn count(db: &TestDb, table: &str) -> i64 {
    db.rows(&format!("SELECT count(*)::text FROM {table}"))
        .await[0][0]
        .parse()
        .unwrap()
}

// ---------------------------------------------------------------------------------------------
// Export
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn export_needs_read_and_export_and_follows_if_owner() {
    let (db, registry) = setup().await;
    add_parcel(&db, &registry, &Caller::system("ann"), "ann-1").await;
    add_parcel(&db, &registry, &Caller::system("bob"), "bob-1").await;
    let run = |caller: Caller| {
        let (db, registry) = (&db, &registry);
        async move { export_all(db, registry, &caller, &[], &[], 100).await }
    };
    let denied = |result: Result<_, DbError>, right: &str| match result {
        Err(DbError::PermissionDenied { right: got, .. }) => assert_eq!(got.name(), right),
        Err(other) => panic!("expected denied {right}, got {other}"),
        Ok(_) => panic!("expected denied {right}"),
    };

    // Read without export, export without read, and neither: refused, read first.
    denied(run(user(&["Plain Reader"])).await.map(|_| ()), "export");
    denied(run(user(&["Exporter"])).await.map(|_| ()), "read");
    denied(run(user(&[])).await.map(|_| ()), "read");
    denied(run(user(&["Create Only"])).await.map(|_| ()), "read");
    // `Reader` reads everything but may export only its own: allowed, limited to own rows.
    let (docs, _, count) = run(user(&["Reader"])).await.unwrap();
    assert_eq!((titles(&docs), count), (vec![], 0));

    // Both rights: everything.
    let (docs, _, count) = run(user(&["Reader Exporter"])).await.unwrap();
    assert_eq!(
        (titles(&docs), count),
        (vec!["ann-1".into(), "bob-1".into()], 2)
    );

    // Read on everything, export on own documents only: the export is limited to own documents.
    let ann_reader = Caller::user("ann", vec!["Reader".to_owned()]);
    let (docs, _, count) = run(ann_reader).await.unwrap();
    assert_eq!((titles(&docs), count), (vec!["ann-1".into()], 1));
    // Read and export both for own documents only.
    let bob = Caller::user("bob", vec!["Own Reader Exporter".to_owned()]);
    let (docs, _, count) = run(bob).await.unwrap();
    assert_eq!((titles(&docs), count), (vec!["bob-1".into()], 1));
    let nobody = other(&["Own Reader Exporter"]);
    assert!(run(nobody).await.unwrap().0.is_empty());

    // Administrator and the system context need no rule.
    let admin = Caller::user("root", vec!["Administrator".to_owned()]);
    assert_eq!(run(admin).await.unwrap().0.len(), 2);
    assert_eq!(run(Caller::system("cli")).await.unwrap().0.len(), 2);
}

#[tokio::test]
async fn export_applies_filters_and_sort_and_counts_the_same_rows() {
    let (db, registry) = setup().await;
    let system = Caller::system("t");
    for (title, qty) in [("b", 2), ("a", 5), ("c", 2), ("d", 9)] {
        let mut client = db.pool.get().await.unwrap();
        let doc = Document {
            name: None,
            values: BTreeMap::from([
                ("title".to_owned(), text(title)),
                ("qty".to_owned(), Value::Int(qty)),
            ]),
            tables: BTreeMap::new(),
        };
        insert(&mut **client, &registry, "parcel", doc, &system)
            .await
            .unwrap();
    }
    let filters = [Filter::new("qty", FilterOp::Lt(Value::Int(9)))];
    let sort = [Sort::desc("qty"), Sort::asc("title")];
    let (docs, pages, count) = export_all(&db, &registry, &system, &filters, &sort, 100)
        .await
        .unwrap();
    assert_eq!(titles(&docs), ["a", "b", "c"]);
    assert_eq!((pages, count), (1, 3));
    // A bad filter is an invalid query, nothing is read.
    let bad = [Filter::new("nope", FilterOp::IsNull(true))];
    let error = export_all(&db, &registry, &system, &bad, &[], 100)
        .await
        .map(|_| ())
        .unwrap_err();
    assert!(matches!(error, DbError::InvalidQuery(_)), "{error}");
}

#[tokio::test]
async fn export_over_the_limit_is_refused_with_the_count() {
    let (db, registry) = setup().await;
    let system = Caller::system("t");
    for i in 0..5 {
        add_parcel(&db, &registry, &system, &format!("p{i}")).await;
    }
    let error = export_all(&db, &registry, &system, &[], &[], 4)
        .await
        .map(|_| ())
        .unwrap_err();
    assert!(
        matches!(error, DbError::ExportTooLarge { count: 5, limit: 4 }),
        "{error}"
    );
    // At the limit it passes.
    assert_eq!(
        export_all(&db, &registry, &system, &[], &[], 5)
            .await
            .unwrap()
            .2,
        5
    );
}

#[tokio::test]
async fn export_streams_several_pages_with_their_child_rows_from_one_snapshot() {
    let (db, registry) = setup().await;
    let system = Caller::system("t");
    // 2500 documents (three pages), each with two child rows, in one statement.
    db.exec(
        "INSERT INTO doc_parcel (id, name, owner, created_at, modified_at, modified_by, title) \
         SELECT gen_random_uuid(), 'P' || lpad(g::text, 5, '0'), 'seed', now(), now(), 'seed', 'T' || g FROM generate_series(1, 2500) g; \
         INSERT INTO doc_parcel_line (id, parent_id, idx, item) \
         SELECT gen_random_uuid(), p.id, i, p.name || '/' || i FROM doc_parcel p, generate_series(1, 2) i;",
    )
    .await;
    let mut client = db.pool.get().await.unwrap();
    let tx = begin_export(&mut client).await.unwrap();
    let query = ExportQuery {
        doctype: "parcel",
        filters: &[],
        sort: &[],
        limit: 100_000,
        with_tables: true,
    };
    let mut export = Export::start(&tx, &registry, &system, &query)
        .await
        .unwrap();
    assert_eq!(export.count(), 2500);
    // A write that commits after the snapshot started is not seen.
    db.exec(
        "INSERT INTO doc_parcel (id, name, owner, created_at, modified_at, modified_by, title) \
         VALUES (gen_random_uuid(), 'P99999', 'seed', now(), now(), 'seed', 'late')",
    )
    .await;
    let mut sizes = Vec::new();
    let mut names = Vec::new();
    while let Some(page) = export.next_page(&tx, &registry).await.unwrap() {
        sizes.push(page.len());
        for doc in &page {
            let rows = &doc.tables["lines"];
            assert_eq!(rows.len(), 2, "{}", doc.name);
            assert_eq!(rows[0]["item"], text(&format!("{}/1", doc.name)));
            assert_eq!(rows[1]["item"], text(&format!("{}/2", doc.name)));
            names.push(doc.name.clone());
        }
    }
    let page = PAGE_ROWS as usize;
    assert_eq!(sizes, [page, page, 2500 - 2 * page]);
    assert_eq!(names.len(), 2500);
    assert!(names.windows(2).all(|w| w[0] < w[1]), "ordered by name");
    assert!(!names.contains(&"P99999".to_owned()));
}

// ---------------------------------------------------------------------------------------------
// Import
// ---------------------------------------------------------------------------------------------

async fn run_import(
    db: &TestDb,
    registry: &Registry,
    doctype: &str,
    caller: &Caller,
    rows: &[ImportRow],
    mode: ImportMode,
    commit: bool,
) -> Result<forgebase_db::ImportOutcome, DbError> {
    let mut client = db.pool.get().await.unwrap();
    import(&mut **client, registry, doctype, caller, rows, mode, commit).await
}

#[tokio::test]
async fn import_needs_import_and_create_and_for_an_upsert_write() {
    let (db, registry) = setup().await;
    let rows = [row(1, "x")];
    let denied = |error: DbError, right: &str| match error {
        DbError::PermissionDenied { right: got, .. } => assert_eq!(got.name(), right),
        other => panic!("expected denied {right}, got {other}"),
    };
    let go = |roles: &'static [&'static str], mode| {
        let (db, registry, rows) = (&db, &registry, &rows);
        async move { run_import(db, registry, "parcel", &user(roles), rows, mode, true).await }
    };
    denied(go(&[], ImportMode::Insert).await.unwrap_err(), "import");
    denied(
        go(&["Create Only"], ImportMode::Insert).await.unwrap_err(),
        "import",
    );
    denied(
        go(&["Import Only"], ImportMode::Insert).await.unwrap_err(),
        "create",
    );
    // Import and create inserts; the same caller may not upsert.
    assert_eq!(
        go(&["Importer"], ImportMode::Insert)
            .await
            .unwrap()
            .inserted,
        1
    );
    denied(
        go(&["Importer"], ImportMode::Upsert).await.unwrap_err(),
        "write",
    );
    let named_rows = [named(1, "N1", &[("title", text("y"))])];
    let ok = run_import(
        &db,
        &registry,
        "parcel",
        &user(&["Upserter"]),
        &named_rows,
        ImportMode::Upsert,
        true,
    )
    .await;
    // `parcel` names are random: a given name is refused as a row error, not a right problem.
    assert_eq!(ok.unwrap().error_count, 1);
    // The administrator and the system context need no rule.
    let admin = Caller::user("root", vec!["Administrator".to_owned()]);
    let outcome = run_import(
        &db,
        &registry,
        "parcel",
        &admin,
        &rows,
        ImportMode::Insert,
        true,
    )
    .await
    .unwrap();
    assert!(outcome.committed);
    // A rights refusal wrote nothing: only the two committed rows exist.
    assert_eq!(count(&db, "doc_parcel").await, 2);
}

#[tokio::test]
async fn one_bad_row_rolls_back_everything_and_every_error_has_its_row() {
    let (db, registry) = setup().await;
    let system = Caller::system("t");
    let mut rows: Vec<ImportRow> = (1..=5).map(|n| row(n, &format!("ok{n}"))).collect();
    rows.push(row(6, "reject")); // a hook refuses
    rows.push(ImportRow {
        row: 7,
        line: Some(8),
        values: BTreeMap::new(), // title is required
        ..ImportRow::default()
    });
    rows.push(ImportRow {
        row: 8,
        values: BTreeMap::from([("title".to_owned(), Value::Int(3))]), // wrong kind
        ..ImportRow::default()
    });
    rows.push(row(9, "fine"));
    let outcome = run_import(
        &db,
        &registry,
        "parcel",
        &system,
        &rows,
        ImportMode::Insert,
        true,
    )
    .await
    .unwrap();
    assert!(!outcome.committed);
    assert_eq!(outcome.inserted, 6);
    assert_eq!(outcome.error_count, 3);
    let numbers: Vec<usize> = outcome.errors.iter().map(|e| e.row).collect();
    assert_eq!(numbers, [6, 7, 8]);
    assert_eq!(outcome.errors[1].line, Some(8));
    assert!(matches!(outcome.errors[0].error, DbError::Hook { .. }));
    assert!(matches!(outcome.errors[1].error, DbError::Validation(_)));
    // Nothing was kept: no documents, no child rows, no audit jobs.
    assert_eq!(count(&db, "doc_parcel").await, 0);
    assert_eq!(count(&db, "fb_job").await, 0);
}

#[tokio::test]
async fn a_dry_run_changes_nothing_not_even_jobs_or_webhook_deliveries() {
    let (db, registry) = setup().await;
    let system = Caller::system("t");
    {
        let client = db.pool.get().await.unwrap();
        let events = vec!["insert".to_owned()];
        webhooks::add(
            &**client,
            &registry,
            &NewWebhook {
                name: "wh",
                doctype: "parcel",
                events: &events,
                url: "https://example.com/hook",
                allow_private: false,
                insecure_http: false,
            },
            None,
        )
        .await
        .unwrap();
    }
    let rows = [row(1, "a"), row(2, "b"), row(3, "hook-me")];
    let dry = run_import(
        &db,
        &registry,
        "parcel",
        &system,
        &rows,
        ImportMode::Insert,
        false,
    )
    .await
    .unwrap();
    assert_eq!(
        (dry.inserted, dry.error_count, dry.committed),
        (3, 0, false)
    );
    assert_eq!(count(&db, "doc_parcel").await, 0);
    assert_eq!(count(&db, "fb_job").await, 0);
    // The same file for real: the documents, the audit jobs (one per row) and one webhook
    // delivery per row exist, all from the one commit, and the hook ran.
    let real = run_import(
        &db,
        &registry,
        "parcel",
        &system,
        &rows,
        ImportMode::Insert,
        true,
    )
    .await
    .unwrap();
    assert!(real.committed);
    assert_eq!(count(&db, "doc_parcel").await, 3);
    let jobs = db
        .rows("SELECT name || '/' || count(*) FROM fb_job GROUP BY name ORDER BY name")
        .await;
    assert_eq!(jobs, [["app.audit/3"], ["forgebase.deliver_webhook/3"]]);
    let notes = db
        .rows("SELECT coalesce(note, '-') FROM doc_parcel WHERE title = 'hook-me'")
        .await;
    assert_eq!(notes, [["hooked"]]);
    // A failed import (any bad row) commits no webhook delivery either.
    let before = count(&db, "fb_job").await;
    let bad = [row(1, "c"), row(2, "reject")];
    let failed = run_import(
        &db,
        &registry,
        "parcel",
        &system,
        &bad,
        ImportMode::Insert,
        true,
    )
    .await
    .unwrap();
    assert_eq!(failed.error_count, 1);
    assert_eq!(count(&db, "fb_job").await, before);
}

#[tokio::test]
async fn errors_are_capped_but_all_are_counted() {
    let (db, registry) = setup().await;
    let rows: Vec<ImportRow> = (1..=150).map(|n| row(n, "reject")).collect();
    let outcome = run_import(
        &db,
        &registry,
        "parcel",
        &Caller::system("t"),
        &rows,
        ImportMode::Insert,
        true,
    )
    .await
    .unwrap();
    assert_eq!(outcome.error_count, 150);
    assert_eq!(outcome.errors.len(), MAX_REPORTED_ERRORS);
    assert_eq!(outcome.errors[99].row, 100);
}

#[tokio::test]
async fn upsert_updates_what_exists_inserts_what_does_not_and_sets_empty_cells_to_null() {
    let (db, registry) = setup().await;
    let system = Caller::system("t");
    let seed = |code: &'static str, level: i64| {
        let (db, registry) = (&db, &registry);
        async move {
            let mut client = db.pool.get().await.unwrap();
            let doc = Document {
                name: Some(code.to_owned()),
                values: BTreeMap::from([
                    ("code".to_owned(), text(code)),
                    ("level".to_owned(), Value::Int(level)),
                ]),
                tables: BTreeMap::new(),
            };
            insert(
                &mut **client,
                registry,
                "badge",
                doc,
                &Caller::system("seed"),
            )
            .await
            .unwrap();
        }
    };
    seed("A", 1).await;
    seed("B", 2).await;
    let rows = [
        // Exists: the level changes; the naming field repeats the name and is left out.
        named(1, "A", &[("code", text("A")), ("level", Value::Int(10))]),
        // Exists, an empty `level` cell: set to null.
        ImportRow {
            empty: vec!["level".to_owned()],
            ..named(2, "B", &[])
        },
        // Does not exist: inserted with that name.
        named(3, "C", &[("code", text("C")), ("level", Value::Int(3))]),
        // The row's naming field names another document: a rename, refused for this row.
        named(4, "A", &[("code", text("Z"))]),
        // No name in an upsert.
        row(5, "x"),
    ];
    let outcome = run_import(
        &db,
        &registry,
        "badge",
        &system,
        &rows,
        ImportMode::Upsert,
        true,
    )
    .await
    .unwrap();
    assert_eq!((outcome.updated, outcome.inserted), (2, 1));
    assert_eq!(outcome.error_count, 2);
    assert_eq!(
        outcome.errors.iter().map(|e| e.row).collect::<Vec<_>>(),
        [4, 5]
    );
    assert!(
        outcome
            .errors
            .iter()
            .all(|e| matches!(e.error, DbError::Naming(_) | DbError::Validation(_)))
    );
    // Rows 4 and 5 failed, so nothing was kept.
    assert!(!outcome.committed);
    let levels = db
        .rows("SELECT name || '=' || coalesce(level::text, 'null') FROM doc_badge ORDER BY name")
        .await;
    assert_eq!(levels, [["A=1"], ["B=2"]]);

    // Without the bad rows it commits.
    let good = &rows[..3];
    let outcome = run_import(
        &db,
        &registry,
        "badge",
        &system,
        good,
        ImportMode::Upsert,
        true,
    )
    .await
    .unwrap();
    assert!(outcome.committed);
    let levels = db
        .rows("SELECT name || '=' || coalesce(level::text, 'null') FROM doc_badge ORDER BY name")
        .await;
    assert_eq!(levels, [["A=10"], ["B=null"], ["C=3"]]);
}

#[tokio::test]
async fn insert_mode_leaves_empty_cells_out_and_refuses_a_name_where_naming_is_automatic() {
    let (db, registry) = setup().await;
    let system = Caller::system("t");
    let rows = [ImportRow {
        empty: vec!["note".to_owned(), "qty".to_owned()],
        ..row(1, "plain")
    }];
    let outcome = run_import(
        &db,
        &registry,
        "parcel",
        &system,
        &rows,
        ImportMode::Insert,
        true,
    )
    .await
    .unwrap();
    assert!(outcome.committed);
    let stored = db
        .rows("SELECT coalesce(note, 'null') || '/' || coalesce(qty::text, 'null') FROM doc_parcel")
        .await;
    assert_eq!(stored, [["null/null"]]);
    // Random naming: a given name is a row error.
    let named_row = [named(1, "mine", &[("title", text("t"))])];
    let outcome = run_import(
        &db,
        &registry,
        "parcel",
        &system,
        &named_row,
        ImportMode::Insert,
        true,
    )
    .await
    .unwrap();
    assert!(matches!(outcome.errors[0].error, DbError::Naming(_)));
    assert_eq!(outcome.errors[0].name.as_deref(), Some("mine"));
    // A naming-field DocType takes the given name when it is the field's value.
    let ok = [named(1, "K1", &[("code", text("K1"))])];
    let outcome = run_import(
        &db,
        &registry,
        "badge",
        &system,
        &ok,
        ImportMode::Insert,
        true,
    )
    .await
    .unwrap();
    assert!(outcome.committed);
    let mismatch = [named(1, "K2", &[("code", text("other"))])];
    let outcome = run_import(
        &db,
        &registry,
        "badge",
        &system,
        &mismatch,
        ImportMode::Insert,
        true,
    )
    .await
    .unwrap();
    assert!(matches!(outcome.errors[0].error, DbError::Naming(_)));
}

#[tokio::test]
async fn an_import_with_child_rows_round_trips_through_the_export() {
    let (db, registry) = setup().await;
    let system = Caller::system("t");
    add_parcel(&db, &registry, &system, "one").await;
    add_parcel(&db, &registry, &system, "two").await;
    let (docs, _, _) = export_all(&db, &registry, &system, &[], &[Sort::asc("title")], 100)
        .await
        .unwrap();
    // Import the exported documents again (insert, new names) and compare values and rows.
    let rows: Vec<ImportRow> = docs
        .iter()
        .enumerate()
        .map(|(i, d)| ImportRow {
            row: i + 1,
            values: d.values.clone(),
            tables: d.tables.clone(),
            ..ImportRow::default()
        })
        .collect();
    let outcome = run_import(
        &db,
        &registry,
        "parcel",
        &system,
        &rows,
        ImportMode::Insert,
        true,
    )
    .await
    .unwrap();
    assert!(outcome.committed, "{:?}", outcome.errors);
    let (again, _, count) = export_all(
        &db,
        &registry,
        &system,
        &[],
        &[Sort::asc("title"), Sort::asc("name")],
        100,
    )
    .await
    .unwrap();
    assert_eq!(count, 4);
    let by_title = |t: &str| -> Vec<&StoredDocument> {
        again
            .iter()
            .filter(|d| d.values["title"] == text(t))
            .collect()
    };
    for original in &docs {
        let Value::Text(t) = &original.values["title"] else {
            unreachable!()
        };
        let copies = by_title(t);
        assert_eq!(copies.len(), 2);
        for copy in copies {
            assert_eq!(copy.values, original.values);
            assert_eq!(copy.tables, original.tables);
        }
    }
    // The documents of the first export are untouched.
    let mut client = db.pool.get().await.unwrap();
    let still = get(&mut **client, &registry, "parcel", &docs[0].name, &system)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(still.version, 1);
}

#[tokio::test]
async fn an_upsert_replaces_the_child_rows_it_is_given_and_keeps_the_others() {
    let (db, registry) = setup().await;
    let system = Caller::system("t");
    // `parcel` is randomly named, so use the stored name of an existing document.
    let doc = add_parcel(&db, &registry, &system, "keep").await;
    let new_rows = vec![Row::from([("item".to_owned(), text("only"))])];
    let rows = [ImportRow {
        row: 1,
        name: Some(doc.name.clone()),
        values: BTreeMap::from([("qty".to_owned(), Value::Int(4))]),
        tables: BTreeMap::from([("lines".to_owned(), new_rows.clone())]),
        ..ImportRow::default()
    }];
    let outcome = run_import(
        &db,
        &registry,
        "parcel",
        &system,
        &rows,
        ImportMode::Upsert,
        true,
    )
    .await
    .unwrap();
    assert_eq!(outcome.updated, 1, "{:?}", outcome.errors);
    let mut client = db.pool.get().await.unwrap();
    let after = get(&mut **client, &registry, "parcel", &doc.name, &system)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.tables["lines"], new_rows);
    assert_eq!(after.values["qty"], Value::Int(4));
    assert_eq!(after.values["title"], text("keep"));
    assert_eq!(after.version, 2);
    // An upsert without the table leaves the rows alone.
    let rows = [ImportRow {
        row: 1,
        name: Some(doc.name.clone()),
        values: BTreeMap::from([("qty".to_owned(), Value::Int(5))]),
        ..ImportRow::default()
    }];
    drop(client);
    run_import(
        &db,
        &registry,
        "parcel",
        &system,
        &rows,
        ImportMode::Upsert,
        true,
    )
    .await
    .unwrap();
    let mut client = db.pool.get().await.unwrap();
    let after = get(&mut **client, &registry, "parcel", &doc.name, &system)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.tables["lines"], new_rows);
}
