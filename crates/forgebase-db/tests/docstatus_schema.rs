//! Schema sync of submittable DocTypes (M2.4): the `docstatus` and `amended_from` columns, adding
//! them to an existing table, drift conflicts and turning `submittable` off.

mod common;

use common::{TestDb, registry};
use forgebase_core::document::DocStatus;
use forgebase_db::{Conflict, DbError, Step, Warning, get};

const PLAIN: &str = r#"
name = "memo"
label = "Memo"
[[fields]]
name = "title"
type = "data"
"#;

const SUBMITTABLE: &str = r#"
name = "memo"
label = "Memo"
submittable = true
[[fields]]
name = "title"
type = "data"
"#;

fn column<'a>(columns: &'a [(String, String)], name: &str) -> Option<&'a str> {
    columns
        .iter()
        .find(|(c, _)| c == name)
        .map(|(_, t)| t.as_str())
}

#[tokio::test]
async fn a_new_submittable_table_has_the_docstatus_columns() {
    let db = TestDb::migrated().await;
    let report = db.sync(&registry(&[SUBMITTABLE])).await.unwrap();
    assert_eq!(report.steps.len(), 3, "{report:?}");
    let Step::CreateTable { sql, .. } = &report.steps[0] else {
        panic!("{report:?}");
    };
    assert!(
        sql.contains("\"docstatus\" smallint NOT NULL DEFAULT 0,\n    \"amended_from\" text,\n"),
        "{sql}"
    );
    let columns = db.columns("doc_memo").await;
    assert_eq!(column(&columns, "docstatus"), Some("smallint/NO"));
    assert_eq!(column(&columns, "amended_from"), Some("text/YES"));
    assert_eq!(
        db.rows(
            "SELECT column_default::text FROM information_schema.columns \
             WHERE table_name = 'doc_memo' AND column_name = 'docstatus'"
        )
        .await,
        vec![vec!["0".to_owned()]]
    );
    // The standard columns come first; a second sync is a no-op.
    assert_eq!(columns[7].0, "docstatus");
    assert!(
        db.sync(&registry(&[SUBMITTABLE]))
            .await
            .unwrap()
            .steps
            .is_empty()
    );
}

#[tokio::test]
async fn a_plain_table_has_no_docstatus_columns() {
    let db = TestDb::migrated().await;
    db.sync(&registry(&[PLAIN])).await.unwrap();
    let columns = db.columns("doc_memo").await;
    assert!(column(&columns, "docstatus").is_none());
    assert!(column(&columns, "amended_from").is_none());
}

#[tokio::test]
async fn making_an_existing_doctype_submittable_adds_the_columns_additively() {
    let db = TestDb::migrated().await;
    let plain = registry(&[PLAIN]);
    db.sync(&plain).await.unwrap();
    db.exec(
        "INSERT INTO doc_memo (id, name, owner, created_at, modified_at, modified_by, title) \
         VALUES (gen_random_uuid(), 'old', 'u', now(), now(), 'u', 'before')",
    )
    .await;

    let submittable = registry(&[SUBMITTABLE]);
    let report = db.sync(&submittable).await.unwrap();
    assert!(report.warnings.is_empty(), "{report:?}");
    assert_eq!(report.steps.len(), 2, "{report:?}");
    assert!(
        matches!(&report.steps[0], Step::AddColumn { column, sql, .. }
            if column == "docstatus"
                && sql == "ALTER TABLE \"doc_memo\" ADD COLUMN \"docstatus\" smallint NOT NULL DEFAULT 0"),
        "{report:?}"
    );
    assert!(
        matches!(&report.steps[1], Step::AddColumn { column, sql, .. }
            if column == "amended_from"
                && sql == "ALTER TABLE \"doc_memo\" ADD COLUMN \"amended_from\" text"),
        "{report:?}"
    );
    let columns = db.columns("doc_memo").await;
    assert_eq!(column(&columns, "docstatus"), Some("smallint/NO"));
    assert_eq!(column(&columns, "amended_from"), Some("text/YES"));

    // The document that was there is now a draft, and reads as one.
    let mut client = db.pool.get().await.unwrap();
    let stored = get(&mut **client, &submittable, "memo", "old")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.docstatus, Some(DocStatus::Draft));
    assert_eq!(stored.amended_from, None);
    assert!(db.sync(&submittable).await.unwrap().steps.is_empty());
}

#[tokio::test]
async fn a_dry_run_shows_the_alter_statements_and_changes_nothing() {
    let db = TestDb::migrated().await;
    db.sync(&registry(&[PLAIN])).await.unwrap();
    let mut client = db.pool.get().await.unwrap();
    let options = forgebase_db::SyncOptions {
        dry_run: true,
        ..forgebase_db::SyncOptions::default()
    };
    let report = forgebase_db::sync(&mut client, &registry(&[SUBMITTABLE]), &options)
        .await
        .unwrap();
    assert_eq!(report.steps.len(), 2);
    drop(client);
    assert!(column(&db.columns("doc_memo").await, "docstatus").is_none());
}

#[tokio::test]
async fn turning_submittable_off_is_a_warning_and_keeps_the_columns() {
    let db = TestDb::migrated().await;
    db.sync(&registry(&[SUBMITTABLE])).await.unwrap();
    let report = db.sync(&registry(&[PLAIN])).await.unwrap();
    assert!(report.steps.is_empty(), "{report:?}");
    for column in ["docstatus", "amended_from"] {
        assert!(
            report.warnings.contains(&Warning::ColumnNotInMetadata {
                table: "doc_memo".into(),
                column: column.into(),
            }),
            "{report:?}"
        );
    }
    let columns = db.columns("doc_memo").await;
    assert!(
        column(&columns, "docstatus").is_some(),
        "the columns are kept"
    );
    // Documents of the plain DocType can still be written (the column has a default).
    let plain = registry(&[PLAIN]);
    let mut client = db.pool.get().await.unwrap();
    let doc = forgebase_core::document::Document::default();
    let stored = forgebase_db::insert(&mut **client, &plain, "memo", doc, "ann")
        .await
        .unwrap();
    assert_eq!(stored.docstatus, None);
}

#[tokio::test]
async fn drift_in_the_docstatus_columns_is_a_conflict() {
    let cases = [
        // (statement that creates the drift, column, expected, actual)
        (
            "ALTER TABLE doc_memo ALTER COLUMN docstatus DROP DEFAULT, \
             ALTER COLUMN docstatus TYPE bigint",
            "docstatus",
            "smallint NOT NULL",
            "bigint NOT NULL",
        ),
        (
            "ALTER TABLE doc_memo ALTER COLUMN docstatus DROP NOT NULL",
            "docstatus",
            "smallint NOT NULL",
            "smallint",
        ),
        (
            "ALTER TABLE doc_memo ALTER COLUMN amended_from SET NOT NULL",
            "amended_from",
            "text",
            "text NOT NULL",
        ),
        (
            "ALTER TABLE doc_memo ALTER COLUMN amended_from TYPE character varying(10)",
            "amended_from",
            "text",
            "character varying(10)",
        ),
    ];
    for (drift, column, expected, actual) in cases {
        let db = TestDb::migrated().await;
        let reg = registry(&[SUBMITTABLE]);
        db.sync(&reg).await.unwrap();
        db.exec(drift).await;
        let err = db.sync(&reg).await.unwrap_err();
        let DbError::SyncConflicts(conflicts) = &err else {
            panic!("{drift}: {err}");
        };
        assert_eq!(
            conflicts,
            &[Conflict::ColumnMismatch {
                table: "doc_memo".into(),
                column: column.into(),
                expected: expected.into(),
                actual: actual.into(),
            }],
            "{drift}"
        );
    }
}

#[tokio::test]
async fn a_dropped_docstatus_column_is_added_again() {
    let db = TestDb::migrated().await;
    let reg = registry(&[SUBMITTABLE]);
    db.sync(&reg).await.unwrap();
    db.exec("ALTER TABLE doc_memo DROP COLUMN amended_from")
        .await;
    let report = db.sync(&reg).await.unwrap();
    assert!(
        matches!(&report.steps[..], [Step::AddColumn { column, .. }] if column == "amended_from"),
        "{report:?}"
    );
}
