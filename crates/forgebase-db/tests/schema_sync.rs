//! Schema sync against a real PostgreSQL (see `common::TestDb`).

mod common;

use std::time::Duration;

use common::{CUSTOMER, TestDb, registry};
use forgebase_db::{Conflict, DbError, Step, SyncOptions, Warning, migrate, sync};

#[tokio::test]
async fn sync_creates_table_with_exact_column_types() {
    let db = TestDb::migrated().await;
    let report = db.sync(&registry(&[CUSTOMER])).await.unwrap();
    assert!(matches!(
        &report.steps[..],
        [
            Step::CreateTable { .. },
            Step::AddKeys { .. },
            Step::RegisterDocType { .. }
        ]
    ));
    let columns: Vec<(&str, &str)> = [
        ("id", "uuid/NO"),
        ("name", "text/NO"),
        ("owner", "text/NO"),
        ("created_at", "timestamp with time zone/NO"),
        ("modified_at", "timestamp with time zone/NO"),
        ("modified_by", "text/NO"),
        ("version", "bigint/NO"),
        ("email", "character varying(100)/YES"),
        ("notes", "text/YES"),
        ("points", "bigint/YES"),
        ("credit_limit", "numeric(12,2)/YES"),
        ("active", "boolean/YES"),
        ("joined_on", "date/YES"),
        ("last_seen", "timestamp with time zone/YES"),
        ("tier", "text/YES"),
    ]
    .to_vec();
    let actual = db.columns("doc_customer").await;
    let actual: Vec<(&str, &str)> = actual
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect();
    assert_eq!(actual, columns);
    let unique = db
        .rows("SELECT indexname::text FROM pg_indexes WHERE tablename = 'doc_customer' ORDER BY 1")
        .await;
    assert_eq!(
        unique,
        [
            ["doc_customer_email_key"],
            ["doc_customer_name_key"],
            ["doc_customer_pkey"]
        ]
    );
    assert_eq!(
        db.rows("SELECT name::text, table_name::text FROM fb_doctype")
            .await,
        vec![vec!["customer".to_owned(), "doc_customer".into()]]
    );
}

#[tokio::test]
async fn resync_is_a_no_op() {
    let db = TestDb::migrated().await;
    let registry = registry(&[CUSTOMER]);
    db.sync(&registry).await.unwrap();
    let again = db.sync(&registry).await.unwrap();
    assert!(
        again.steps.is_empty() && again.warnings.is_empty(),
        "{again:?}"
    );
}

#[tokio::test]
async fn dry_run_creates_nothing_and_reports_sql() {
    let db = TestDb::migrated().await;
    let mut client = db.pool.get().await.unwrap();
    let options = SyncOptions {
        dry_run: true,
        ..SyncOptions::default()
    };
    let report = sync(&mut client, &registry(&[CUSTOMER]), &options)
        .await
        .unwrap();
    assert!(report.dry_run && report.steps.len() == 3);
    assert!(
        report.steps[0]
            .to_string()
            .starts_with("CREATE TABLE \"doc_customer\"")
    );
    assert!(db.columns("doc_customer").await.is_empty());
    assert_eq!(
        db.rows("SELECT count(*)::text FROM fb_doctype").await,
        vec![vec!["0".to_owned()]]
    );
}

#[tokio::test]
async fn added_field_becomes_nullable_column_and_unique_becomes_index() {
    let db = TestDb::migrated().await;
    db.sync(&registry(&[CUSTOMER])).await.unwrap();
    db.exec(
        "INSERT INTO doc_customer (id, name, owner, created_at, modified_at, modified_by) \
             VALUES (gen_random_uuid(), 'a', 'u', now(), now(), 'u')",
    )
    .await;

    let extended = format!(
        "{CUSTOMER}\n[[fields]]\nname = \"phone\"\ntype = \"data\"\nunique = true\n\n[[fields]]\nname = \"city\"\ntype = \"data\"\n"
    );
    let report = db.sync(&registry(&[&extended])).await.unwrap();
    assert_eq!(report.steps.len(), 3, "{report:?}");
    // Phases: all added columns first, then the late unique indexes.
    assert!(matches!(&report.steps[0], Step::AddColumn { column, .. } if column == "phone"));
    assert!(matches!(&report.steps[1], Step::AddColumn { column, .. } if column == "city"));
    assert!(
        matches!(&report.steps[2], Step::CreateUniqueIndex { column, .. } if column == "phone")
    );
    let columns = db.columns("doc_customer").await;
    assert!(columns.contains(&("phone".into(), "character varying(140)/YES".into())));
    assert!(columns.contains(&("city".into(), "character varying(140)/YES".into())));
    let index = db
        .rows("SELECT indexdef::text FROM pg_indexes WHERE tablename = 'doc_customer' AND indexdef LIKE '%(phone)'")
        .await;
    assert!(index[0][0].contains("CREATE UNIQUE INDEX"), "{index:?}");

    // An existing column that becomes unique later also gets its index.
    let later = extended.replace(
        "name = \"city\"\ntype = \"data\"",
        "name = \"city\"\ntype = \"data\"\nunique = true",
    );
    let report = db.sync(&registry(&[&later])).await.unwrap();
    assert!(
        matches!(&report.steps[..], [Step::CreateUniqueIndex { column, .. }] if column == "city")
    );
}

#[tokio::test]
async fn data_max_length_change_is_a_conflict_and_changes_nothing() {
    let db = TestDb::migrated().await;
    db.sync(&registry(&[CUSTOMER])).await.unwrap();
    // One conflicting change and one otherwise-additive change in the same sync.
    let changed = format!(
        "{}\n[[fields]]\nname = \"extra\"\ntype = \"text\"\n",
        CUSTOMER.replace("max_length = 100", "max_length = 200")
    );
    let err = db.sync(&registry(&[&changed])).await.unwrap_err();
    let DbError::SyncConflicts(conflicts) = &err else {
        panic!("{err}")
    };
    assert_eq!(
        conflicts,
        &[Conflict::ColumnMismatch {
            table: "doc_customer".into(),
            column: "email".into(),
            expected: "character varying(200)".into(),
            actual: "character varying(100)".into(),
        }]
    );
    assert!(err.to_string().contains("doc_customer.email"), "{err}");
    let columns = db.columns("doc_customer").await;
    assert!(columns.contains(&("email".into(), "character varying(100)/YES".into())));
    assert!(
        !columns.iter().any(|(c, _)| c == "extra"),
        "additive step leaked past a conflict"
    );
}

#[tokio::test]
async fn removed_field_is_a_warning_and_column_is_kept() {
    let db = TestDb::migrated().await;
    db.sync(&registry(&[CUSTOMER])).await.unwrap();
    let without_notes = CUSTOMER.replace("[[fields]]\nname = \"notes\"\ntype = \"text\"\n", "");
    let without_unique = without_notes.replace("unique = true", "");
    let report = db.sync(&registry(&[&without_unique])).await.unwrap();
    assert!(report.steps.is_empty());
    assert!(report.warnings.contains(&Warning::ColumnNotInMetadata {
        table: "doc_customer".into(),
        column: "notes".into()
    }));
    assert!(report.warnings.contains(&Warning::UniqueRemoved {
        table: "doc_customer".into(),
        column: "email".into()
    }));
    assert!(
        db.columns("doc_customer")
            .await
            .iter()
            .any(|(c, _)| c == "notes")
    );
}

#[tokio::test]
async fn doctype_missing_from_registry_is_a_warning() {
    let db = TestDb::migrated().await;
    db.sync(&registry(&[CUSTOMER])).await.unwrap();
    let report = db.sync(&registry(&[])).await.unwrap();
    assert_eq!(
        report.warnings,
        vec![Warning::DocTypeNotInRegistry {
            doctype: "customer".into()
        }]
    );
    assert!(!db.columns("doc_customer").await.is_empty());
}

#[tokio::test]
async fn unmanaged_existing_table_is_a_conflict() {
    let db = TestDb::migrated().await;
    db.exec("CREATE TABLE doc_customer (id int)").await;
    let err = db.sync(&registry(&[CUSTOMER])).await.unwrap_err();
    assert!(
        matches!(&err, DbError::SyncConflicts(c) if c == &[Conflict::UnmanagedTable { table: "doc_customer".into() }]),
        "{err}"
    );
    assert_eq!(db.columns("doc_customer").await.len(), 1);
}

#[tokio::test]
async fn non_table_relation_with_the_table_name_is_a_conflict() {
    let db = TestDb::migrated().await;
    db.exec("CREATE SEQUENCE doc_customer").await;
    let err = db.sync(&registry(&[CUSTOMER])).await.unwrap_err();
    assert!(
        matches!(&err, DbError::SyncConflicts(c) if c == &[Conflict::RelationNameTaken { name: "doc_customer".into() }]),
        "{err}"
    );
    assert!(err.to_string().contains("doc_customer"), "{err}");
}

const A_B: &str =
    "name = \"a_b\"\nlabel = \"L\"\n[[fields]]\nname = \"c\"\ntype = \"data\"\nunique = true\n";
const A: &str =
    "name = \"a\"\nlabel = \"L\"\n[[fields]]\nname = \"b_c\"\ntype = \"data\"\nunique = true\n";

#[tokio::test]
async fn ambiguous_generated_names_do_not_collide() {
    // `a_b` + `c` and `a` + `b_c` would both be called `doc_a_b_c_key` under a naming scheme of
    // our own; PostgreSQL picks distinct names. Together, then one after the other.
    let together = TestDb::migrated().await;
    together.sync(&registry(&[A_B, A])).await.unwrap();
    let after = TestDb::migrated().await;
    after.sync(&registry(&[A_B])).await.unwrap();
    after.sync(&registry(&[A_B, A])).await.unwrap();
    for db in [&together, &after] {
        let unique = db
            .rows("SELECT count(*)::text FROM pg_indexes WHERE tablename IN ('doc_a', 'doc_a_b') AND indexdef LIKE 'CREATE UNIQUE%'")
            .await;
        // pkey + name + field for each of the two tables.
        assert_eq!(unique, vec![vec!["6".to_owned()]]);
        assert!(
            db.sync(&registry(&[A_B, A]))
                .await
                .unwrap()
                .steps
                .is_empty()
        );
    }
    // The unique index added to an existing table later is named by PostgreSQL too.
    let late = TestDb::migrated().await;
    let without_unique = A.replace("unique = true", "");
    late.sync(&registry(&[A_B, &without_unique])).await.unwrap();
    let report = late.sync(&registry(&[A_B, A])).await.unwrap();
    assert!(matches!(
        &report.steps[..],
        [Step::CreateUniqueIndex { .. }]
    ));
}

fn bare(name: &str) -> String {
    format!("name = \"{name}\"\nlabel = \"L\"\n")
}

const ITEM: &str =
    "name = \"item\"\nlabel = \"L\"\n[[fields]]\nname = \"code\"\ntype = \"data\"\nunique = true\n";

#[tokio::test]
async fn doctype_named_like_a_generated_index_syncs_or_reports_a_conflict() {
    for clash in ["item_code_key", "item_pkey", "item_name_key"] {
        // Together in one sync: tables are created so that PostgreSQL can pick another index name.
        let db = TestDb::migrated().await;
        let reg = registry(&[ITEM, &bare(clash)]);
        db.sync(&reg).await.unwrap();
        assert!(db.sync(&reg).await.unwrap().steps.is_empty(), "{clash}");

        // The DocType added after `item` already owns the index name: a clear conflict.
        let db = TestDb::migrated().await;
        db.sync(&registry(&[ITEM])).await.unwrap();
        let err = db.sync(&registry(&[ITEM, &bare(clash)])).await.unwrap_err();
        let DbError::SyncConflicts(conflicts) = &err else {
            panic!("{clash}: {err}")
        };
        assert_eq!(
            conflicts,
            &[Conflict::RelationNameTaken {
                name: format!("doc_{clash}")
            }]
        );
        // Nothing else was applied, and the existing schema still re-syncs cleanly.
        assert!(db.sync(&registry(&[ITEM])).await.unwrap().steps.is_empty());
    }
}

#[tokio::test]
async fn standard_column_drift_is_a_conflict() {
    let db = TestDb::migrated().await;
    db.sync(&registry(&[CUSTOMER])).await.unwrap();
    db.exec("ALTER TABLE doc_customer ALTER COLUMN owner DROP NOT NULL")
        .await;
    let err = db.sync(&registry(&[CUSTOMER])).await.unwrap_err();
    assert!(
        matches!(&err, DbError::SyncConflicts(c) if c.len() == 1),
        "{err}"
    );
}

#[tokio::test]
async fn standard_key_drift_is_a_conflict() {
    let db = TestDb::migrated().await;
    db.sync(&registry(&[CUSTOMER])).await.unwrap();
    db.exec("ALTER TABLE doc_customer DROP CONSTRAINT doc_customer_name_key")
        .await;
    let err = db.sync(&registry(&[CUSTOMER])).await.unwrap_err();
    assert!(
        matches!(&err, DbError::SyncConflicts(c) if matches!(&c[..], [Conflict::ColumnMismatch { column, expected, .. }] if column == "name" && expected == "unique")),
        "{err}"
    );
    db.exec("ALTER TABLE doc_customer ADD UNIQUE (name); ALTER TABLE doc_customer DROP CONSTRAINT doc_customer_pkey")
        .await;
    let err = db.sync(&registry(&[CUSTOMER])).await.unwrap_err();
    assert!(
        matches!(&err, DbError::SyncConflicts(c) if matches!(&c[..], [Conflict::ColumnMismatch { column, expected, .. }] if column == "id" && expected == "primary key")),
        "{err}"
    );
}

#[tokio::test]
async fn not_null_field_column_warns_and_timestamp_precision_conflicts() {
    let db = TestDb::migrated().await;
    db.sync(&registry(&[CUSTOMER])).await.unwrap();
    db.exec("ALTER TABLE doc_customer ALTER COLUMN notes SET NOT NULL")
        .await;
    let report = db.sync(&registry(&[CUSTOMER])).await.unwrap();
    assert_eq!(
        report.warnings,
        vec![Warning::ColumnNotNullable {
            table: "doc_customer".into(),
            column: "notes".into()
        }]
    );
    db.exec("ALTER TABLE doc_customer ALTER COLUMN last_seen TYPE timestamptz(0)")
        .await;
    let err = db.sync(&registry(&[CUSTOMER])).await.unwrap_err();
    assert!(
        matches!(&err, DbError::SyncConflicts(c) if matches!(&c[..], [Conflict::ColumnMismatch { column, actual, .. }] if column == "last_seen" && actual == "timestamp(0) with time zone")),
        "{err}"
    );
}

#[tokio::test]
async fn adding_unique_over_duplicates_fails_and_applies_nothing() {
    let db = TestDb::migrated().await;
    let plain = CUSTOMER.replace("unique = true", "");
    db.sync(&registry(&[&plain])).await.unwrap();
    for name in ["a", "b"] {
        db.exec(&format!(
            "INSERT INTO doc_customer (id, name, owner, created_at, modified_at, modified_by, email) \
             VALUES (gen_random_uuid(), '{name}', 'u', now(), now(), 'u', 'same@x')"
        ))
        .await;
    }
    let changed = format!("{CUSTOMER}\n[[fields]]\nname = \"extra\"\ntype = \"text\"\n");
    let err = db.sync(&registry(&[&changed])).await.unwrap_err();
    assert!(matches!(err, DbError::Postgres(_)), "{err}");
    assert!(err.to_string().contains("23505"), "{err}");
    assert!(
        !db.columns("doc_customer")
            .await
            .iter()
            .any(|(c, _)| c == "extra"),
        "a step of the failed sync was kept"
    );
}

#[tokio::test]
async fn huge_lock_timeout_is_clamped() {
    let db = TestDb::migrated().await;
    let mut client = db.pool.get().await.unwrap();
    let options = SyncOptions {
        dry_run: true,
        lock_timeout: Duration::from_secs(100_000_000),
    };
    sync(&mut client, &registry(&[CUSTOMER]), &options)
        .await
        .unwrap();
}

#[tokio::test]
async fn lock_timeout_reports_a_retryable_error() {
    let db = TestDb::migrated().await;
    db.sync(&registry(&[CUSTOMER])).await.unwrap();

    // Another connection holds a conflicting table lock while we try to add a column.
    let mut blocker = db.pool.get().await.unwrap();
    let tx = blocker.transaction().await.unwrap();
    tx.batch_execute("LOCK TABLE doc_customer IN ACCESS EXCLUSIVE MODE")
        .await
        .unwrap();

    let extended = format!("{CUSTOMER}\n[[fields]]\nname = \"extra\"\ntype = \"text\"\n");
    let mut client = db.pool.get().await.unwrap();
    let options = SyncOptions {
        dry_run: false,
        lock_timeout: Duration::from_millis(200),
    };
    let err = sync(&mut client, &registry(&[&extended]), &options)
        .await
        .unwrap_err();
    assert!(matches!(err, DbError::LockTimeout), "{err}");
    assert!(err.to_string().contains("retry"));
    drop(client);

    tx.rollback().await.unwrap();
    drop(blocker);
    // After the lock is gone the same sync succeeds: the failed one left nothing behind.
    let report = db.sync(&registry(&[&extended])).await.unwrap();
    assert_eq!(report.steps.len(), 1);
}

#[tokio::test]
async fn concurrent_syncs_both_succeed() {
    let db = TestDb::migrated().await;
    let registry = registry(&[CUSTOMER]);
    let (mut a, mut b) = (db.pool.get().await.unwrap(), db.pool.get().await.unwrap());
    let options = SyncOptions::default();
    let (first, second) = tokio::join!(
        sync(&mut a, &registry, &options),
        sync(&mut b, &registry, &options)
    );
    let (first, second) = (first.unwrap(), second.unwrap());
    // Exactly one of them did the work, the other saw it already done.
    assert_eq!(
        first.steps.len() + second.steps.len(),
        3,
        "{first:?} {second:?}"
    );
}

#[tokio::test]
async fn sync_before_framework_migrations_fails_clearly() {
    let db = TestDb::new().await;
    let err = db.sync(&registry(&[CUSTOMER])).await.unwrap_err();
    assert!(matches!(err, DbError::FrameworkNotMigrated), "{err}");
    assert!(err.to_string().contains("migrations"));
}

#[tokio::test]
async fn migrate_runs_migrations_then_sync() {
    let db = TestDb::new().await;
    let registry = registry(&[CUSTOMER]);
    let dry = SyncOptions {
        dry_run: true,
        ..SyncOptions::default()
    };
    let report = migrate(&db.pool, &registry, &dry).await.unwrap();
    assert!(!report.migrations.is_empty() && report.sync.is_none());
    assert!(db.columns("doc_customer").await.is_empty());

    let report = migrate(&db.pool, &registry, &SyncOptions::default())
        .await
        .unwrap();
    assert_eq!(report.migrations.len(), 1);
    assert_eq!(report.sync.unwrap().steps.len(), 3);
    let report = migrate(&db.pool, &registry, &SyncOptions::default())
        .await
        .unwrap();
    assert!(report.migrations.is_empty() && report.sync.unwrap().steps.is_empty());
}

#[tokio::test]
async fn long_doctype_and_field_names_get_postgres_chosen_bounded_names() {
    let db = TestDb::migrated().await;
    let doctype = format!("d{}", "x".repeat(58));
    let field = format!("f{}", "y".repeat(62));
    let toml = format!(
        "name = \"{doctype}\"\nlabel = \"L\"\n[[fields]]\nname = \"{field}\"\ntype = \"data\"\nunique = true\n"
    );
    db.sync(&registry(&[&toml])).await.unwrap();
    let names = db
        .rows("SELECT indexname::text FROM pg_indexes WHERE tablename LIKE 'doc_dxxx%' ORDER BY 1")
        .await;
    assert_eq!(names.len(), 3);
    assert!(names.iter().all(|n| n[0].len() <= 63));
    assert!(db.sync(&registry(&[&toml])).await.unwrap().steps.is_empty());
}

/// A DocType with no fields, or one unique field.
fn long_named(name: &str, unique_field: Option<&str>) -> String {
    let mut toml = bare(name);
    if let Some(field) = unique_field {
        toml += &format!("[[fields]]\nname = \"{field}\"\ntype = \"data\"\nunique = true\n");
    }
    toml
}

#[tokio::test]
async fn truncated_generated_names_never_collide_with_tables_in_one_sync() {
    let x = |n: usize| "x".repeat(n);
    let cases: Vec<(&str, Vec<String>)> = vec![
        // R01: the `name` unique constraint of the first is named like the second table.
        (
            "name_key",
            vec![
                long_named(&x(51), None),
                long_named(&format!("{}_name_key", x(50)), None),
            ],
        ),
        // R02: the primary key index.
        (
            "pkey",
            vec![
                long_named(&x(55), None),
                long_named(&format!("{}_pkey", x(54)), None),
            ],
        ),
        // R03: a unique field.
        (
            "field_key",
            vec![
                long_named(&"y".repeat(50), Some("codex")),
                long_named(&format!("{}_codex_key", "y".repeat(49)), None),
            ],
        ),
        // Cycle: each table's generated name is the other table's name.
        (
            "cycle",
            vec![
                long_named(&format!("{}_name_key", "w".repeat(50)), None),
                long_named(&format!("{}_nam_pkey", "w".repeat(50)), None),
            ],
        ),
    ];
    for (label, tomls) in cases {
        let refs: Vec<&str> = tomls.iter().map(String::as_str).collect();
        let db = TestDb::migrated().await;
        let reg = registry(&refs);
        let report = db
            .sync(&reg)
            .await
            .unwrap_or_else(|e| panic!("{label}: {e}"));
        assert_eq!(report.steps.len(), 3 * refs.len(), "{label}");
        assert!(db.sync(&reg).await.unwrap().steps.is_empty(), "{label}");
    }
}

#[tokio::test]
async fn late_unique_index_does_not_collide_with_a_new_table() {
    // R05: the late index on an existing table is named like a table created in the same sync.
    let v = "v".repeat(50);
    let existing = long_named(&v, None);
    let with_unique = long_named(&v, Some("codex"));
    let clash = long_named(&format!("{}_codex_idx", "v".repeat(49)), None);
    let db = TestDb::migrated().await;
    db.sync(&registry(&[&existing])).await.unwrap();
    let reg = registry(&[&with_unique, &clash]);
    db.sync(&reg).await.unwrap();
    assert!(db.sync(&reg).await.unwrap().steps.is_empty());
}

#[tokio::test]
async fn view_with_the_table_name_is_a_name_conflict_not_unmanaged() {
    let db = TestDb::migrated().await;
    db.exec("CREATE VIEW doc_customer AS SELECT 1 AS id").await;
    let err = db.sync(&registry(&[CUSTOMER])).await.unwrap_err();
    assert!(
        matches!(&err, DbError::SyncConflicts(c) if c == &[Conflict::RelationNameTaken { name: "doc_customer".into() }]),
        "{err}"
    );
}

#[tokio::test]
async fn domain_with_the_table_name_is_a_name_conflict() {
    let db = TestDb::migrated().await;
    db.exec("CREATE DOMAIN doc_customer AS text").await;
    let err = db.sync(&registry(&[CUSTOMER])).await.unwrap_err();
    assert!(
        matches!(&err, DbError::SyncConflicts(c) if c == &[Conflict::RelationNameTaken { name: "doc_customer".into() }]),
        "{err}"
    );
    // A synced table's own row type is not a conflict.
    let db = TestDb::migrated().await;
    db.sync(&registry(&[CUSTOMER])).await.unwrap();
    assert!(
        db.sync(&registry(&[CUSTOMER]))
            .await
            .unwrap()
            .steps
            .is_empty()
    );
}

/// Schema names that need quoting (mixed case, space, dot) must be read by name, never parsed as
/// identifiers, and a lower-case twin schema must not be read or written.
#[tokio::test]
async fn schema_names_needing_quotes_are_handled_by_name() {
    use forgebase_db::{migrate_framework, quote_ident};

    for (schema, twin) in [
        ("MySchema", Some("myschema")),
        ("my schema", None),
        ("a.b", None),
    ] {
        let db = TestDb::new().await;
        let mut client = db.pool.get().await.expect("connection");
        let q = quote_ident(schema);
        client
            .batch_execute(&format!("CREATE SCHEMA {q}"))
            .await
            .unwrap();
        if let Some(twin) = twin {
            // A decoy that must neither conflict nor be modified.
            client
                .batch_execute(&format!(
                    "CREATE SCHEMA {twin}; \
                     CREATE TABLE {twin}.doc_customer (id uuid PRIMARY KEY, name text UNIQUE)"
                ))
                .await
                .unwrap();
        }
        client
            .batch_execute(&format!("SET search_path TO {q}"))
            .await
            .unwrap();
        migrate_framework(&mut client, false).await.unwrap();

        let reg = registry(&[CUSTOMER]);
        let opts = SyncOptions::default();
        let first = sync(&mut client, &reg, &opts).await.unwrap();
        assert!(
            matches!(first.steps[0], Step::CreateTable { .. }),
            "{schema}"
        );
        let again = sync(&mut client, &reg, &opts).await.unwrap();
        assert!(
            again.steps.is_empty() && again.warnings.is_empty(),
            "{schema}"
        );

        let in_schema = client
            .query(
                "SELECT count(*)::text FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE n.nspname = $1 AND c.relname IN ('doc_customer', 'fb_doctype', 'fb_migration')",
                &[&schema],
            )
            .await
            .unwrap();
        assert_eq!(in_schema[0].get::<_, String>(0), "3", "{schema}");
        if let Some(twin) = twin {
            let cols = client
                .query(
                    "SELECT count(*)::text FROM information_schema.columns \
                     WHERE table_schema = $1",
                    &[&twin],
                )
                .await
                .unwrap();
            assert_eq!(cols[0].get::<_, String>(0), "2", "decoy untouched");
            let others = client
                .query(
                    "SELECT count(*)::text FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
                     WHERE n.nspname = $1",
                    &[&twin],
                )
                .await
                .unwrap();
            // doc_customer, its pkey and its name key; nothing else.
            assert_eq!(others[0].get::<_, String>(0), "3", "decoy untouched");
        }
    }
}
