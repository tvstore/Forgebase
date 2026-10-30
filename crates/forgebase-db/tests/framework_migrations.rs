//! Framework migrations against a real PostgreSQL (see `common::TestDb`).

mod common;

use common::{TestDb, url_for_database, url_with_password};
use forgebase_db::{DbError, MIGRATIONS, connect, migrate_framework};

#[tokio::test]
async fn migrates_empty_database_and_is_idempotent() {
    let db = TestDb::new().await;
    let mut client = db.pool.get().await.unwrap();

    let applied = migrate_framework(&mut client, false).await.unwrap();
    assert_eq!(applied.len(), MIGRATIONS.len());
    let rows = db
        .rows("SELECT version::text, name, length(checksum)::text FROM fb_migration")
        .await;
    assert_eq!(
        rows,
        vec![
            vec!["1".to_owned(), "init".into(), "64".into()],
            vec!["2".to_owned(), "series".into(), "64".into()],
            vec!["3".to_owned(), "auth".into(), "64".into()],
            vec!["4".to_owned(), "apps".into(), "64".into()],
            vec!["5".to_owned(), "jobs".into(), "64".into()],
            vec!["6".to_owned(), "schedules".into(), "64".into()],
            vec!["7".to_owned(), "finished_jobs_index".into(), "64".into()],
            vec!["8".to_owned(), "workflow_log".into(), "64".into()],
            vec!["9".to_owned(), "saved_views".into(), "64".into()],
        ]
    );
    assert_eq!(
        db.rows("SELECT count(*)::text FROM fb_doctype").await,
        vec![vec!["0".to_owned()]]
    );

    assert!(
        migrate_framework(&mut client, false)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        db.rows("SELECT count(*)::text FROM fb_migration").await,
        vec![vec!["9".to_owned()]]
    );
}

#[tokio::test]
async fn the_series_counter_table_is_created_by_migration_2() {
    let db = TestDb::migrated().await;
    // `prefix` is the primary key, `current` is not null.
    assert_eq!(
        db.columns("fb_series").await,
        vec![
            ("prefix".to_owned(), "text/NO".to_owned()),
            ("current".to_owned(), "bigint/NO".to_owned()),
        ]
    );
    assert_eq!(
        db.rows(
            "SELECT a.attname::text FROM pg_index i JOIN pg_attribute a ON a.attrelid = i.indrelid \
             AND a.attnum = i.indkey[0] WHERE i.indrelid = 'fb_series'::regclass AND i.indisprimary"
        )
        .await,
        vec![vec!["prefix".to_owned()]]
    );
}

/// Applies the first `count` migrations by hand, with the checksums the runner computes.
async fn apply_first(db: &TestDb, count: usize) {
    db.exec(
        "CREATE TABLE fb_migration (version integer PRIMARY KEY, name text NOT NULL, \
         checksum text NOT NULL, applied_at timestamptz NOT NULL DEFAULT now())",
    )
    .await;
    for migration in &MIGRATIONS[..count] {
        db.exec(migration.sql).await;
        db.exec(&format!(
            "INSERT INTO fb_migration (version, name, checksum) VALUES ({}, '{}', \
             encode(sha256(convert_to($sql${}$sql$, 'UTF8')), 'hex'))",
            migration.version, migration.name, migration.sql
        ))
        .await;
    }
}

#[tokio::test]
async fn a_database_at_migration_1_is_upgraded_to_the_latest() {
    let db = TestDb::new().await;
    let mut client = db.pool.get().await.unwrap();
    apply_first(&db, 1).await;
    let applied = migrate_framework(&mut client, false).await.unwrap();
    assert_eq!(
        applied.iter().map(|m| m.version).collect::<Vec<_>>(),
        vec![2, 3, 4, 5, 6, 7, 8, 9]
    );
    assert_eq!(
        db.rows("SELECT version::text FROM fb_migration ORDER BY version")
            .await,
        vec![
            vec!["1".to_owned()],
            vec!["2".into()],
            vec!["3".into()],
            vec!["4".into()],
            vec!["5".into()],
            vec!["6".into()],
            vec!["7".into()],
            vec!["8".into()],
            vec!["9".into()]
        ]
    );
    assert_eq!(
        db.rows("SELECT count(*)::text FROM fb_series").await,
        vec![vec!["0".to_owned()]]
    );
}

#[tokio::test]
async fn a_database_at_migration_2_is_upgraded_to_the_latest() {
    let db = TestDb::new().await;
    let mut client = db.pool.get().await.unwrap();
    apply_first(&db, 2).await;
    let applied = migrate_framework(&mut client, false).await.unwrap();
    assert_eq!(
        applied.iter().map(|m| m.version).collect::<Vec<_>>(),
        vec![3, 4, 5, 6, 7, 8, 9]
    );
    assert_eq!(applied[0].name, "auth");
    for table in [
        "fb_user",
        "fb_user_role",
        "fb_session",
        "fb_api_token",
        "fb_login_failure",
        "fb_app",
        "fb_app_migration",
        "fb_schedule",
    ] {
        assert_eq!(
            db.rows(&format!("SELECT count(*)::text FROM {table}"))
                .await,
            vec![vec!["0".to_owned()]],
            "{table}"
        );
    }
}

#[tokio::test]
async fn dry_run_applies_nothing() {
    let db = TestDb::new().await;
    let mut client = db.pool.get().await.unwrap();
    let pending = migrate_framework(&mut client, true).await.unwrap();
    assert_eq!(pending.len(), MIGRATIONS.len());
    let tables = db
        .rows("SELECT table_name::text FROM information_schema.tables WHERE table_name LIKE 'fb_%'")
        .await;
    assert!(tables.is_empty(), "dry run left tables behind: {tables:?}");
}

#[tokio::test]
async fn tampered_checksum_is_rejected() {
    let db = TestDb::migrated().await;
    db.exec("UPDATE fb_migration SET checksum = repeat('0', 64)")
        .await;
    let mut client = db.pool.get().await.unwrap();
    let err = migrate_framework(&mut client, false).await.unwrap_err();
    assert!(
        matches!(err, DbError::ChecksumMismatch { version: 1, .. }),
        "{err}"
    );
}

#[tokio::test]
async fn unknown_applied_version_is_rejected() {
    let db = TestDb::migrated().await;
    db.exec(
        "INSERT INTO fb_migration (version, name, checksum) VALUES (999, 'from_the_future', 'x')",
    )
    .await;
    let mut client = db.pool.get().await.unwrap();
    let err = migrate_framework(&mut client, false).await.unwrap_err();
    assert!(
        matches!(&err, DbError::UnknownMigration { version: 999, .. }),
        "{err}"
    );
    assert!(err.to_string().contains("from_the_future"));
}

#[tokio::test]
async fn invalid_url_and_unreachable_server_give_clear_errors() {
    let err = connect("not a url").await.unwrap_err();
    assert!(matches!(err, DbError::CreatePool(_)), "{err}");
    let err = connect("postgres://nobody:nothing@127.0.0.1:1/none")
        .await
        .unwrap_err();
    // A failed connection attempt is a Postgres error carrying the cause, not a bare "db error".
    assert!(matches!(err, DbError::Postgres(_)), "{err}");
    assert!(err.to_string().contains("error connecting"), "{err}");
}

#[tokio::test]
async fn postgres_errors_show_server_message_and_sqlstate() {
    let db = TestDb::migrated().await;
    let client = db.pool.get().await.unwrap();
    // Violates the primary key of fb_migration.
    let err = client
        .batch_execute("INSERT INTO fb_migration VALUES (1, 'dup', 'x')")
        .await
        .map_err(DbError::from)
        .unwrap_err();
    let text = err.to_string();
    assert!(
        text.contains("duplicate key") && text.contains("23505"),
        "{text}"
    );
}

#[tokio::test]
async fn connect_errors_show_server_message_and_sqlstate() {
    let err = connect(&url_with_password("definitely-wrong"))
        .await
        .unwrap_err();
    let text = err.to_string();
    assert!(
        text.contains("password authentication failed") && text.contains("28P01"),
        "{text}"
    );
    assert!(!text.contains("definitely-wrong"), "{text}");

    let err = connect(&url_for_database("fb_no_such_database"))
        .await
        .unwrap_err();
    let text = err.to_string();
    assert!(
        text.contains("does not exist") && text.contains("3D000"),
        "{text}"
    );
}

#[tokio::test]
async fn unexpected_fb_migration_shape_is_an_error_not_a_panic() {
    let db = TestDb::new().await;
    db.exec(
        "CREATE TABLE fb_migration (version bigint PRIMARY KEY, name text NOT NULL, \
         checksum text NOT NULL, applied_at timestamptz NOT NULL DEFAULT now()); \
         INSERT INTO fb_migration (version, name, checksum) VALUES (1, 'init', 'x')",
    )
    .await;
    let mut client = db.pool.get().await.unwrap();
    let err = migrate_framework(&mut client, false).await.unwrap_err();
    assert!(matches!(err, DbError::Postgres(_)), "{err}");
}

#[tokio::test]
async fn migration_7_replaces_the_done_jobs_index_with_a_finished_jobs_index() {
    let db = TestDb::migrated().await;
    assert_eq!(
        db.rows(
            "SELECT indexname FROM pg_indexes \
             WHERE tablename = 'fb_job' AND indexname LIKE 'fb_job_%_idx' \
             AND indexname IN ('fb_job_done_idx', 'fb_job_finished_idx')"
        )
        .await,
        vec![vec!["fb_job_finished_idx".to_owned()]]
    );
}

#[tokio::test]
async fn migration_8_creates_the_workflow_log() {
    let db = TestDb::migrated().await;
    assert_eq!(
        db.columns("fb_workflow_log").await,
        vec![
            ("id".to_owned(), "bigint/NO".to_owned()),
            ("doctype".to_owned(), "text/NO".to_owned()),
            ("name".to_owned(), "text/NO".to_owned()),
            ("action".to_owned(), "text/NO".to_owned()),
            ("from_state".to_owned(), "text/NO".to_owned()),
            ("to_state".to_owned(), "text/NO".to_owned()),
            ("actor".to_owned(), "text/NO".to_owned()),
            ("at".to_owned(), "timestamp with time zone/NO".to_owned()),
        ]
    );
    assert_eq!(
        db.rows(
            "SELECT indexdef FROM pg_indexes WHERE tablename = 'fb_workflow_log' \
             AND indexname = 'fb_workflow_log_document'"
        )
        .await,
        vec![vec![
            "CREATE INDEX fb_workflow_log_document ON public.fb_workflow_log USING btree (doctype, name, id)"
                .to_owned()
        ]]
    );
}

#[tokio::test]
async fn migration_9_creates_the_saved_views() {
    let db = TestDb::migrated().await;
    assert_eq!(
        db.columns("fb_saved_view").await,
        vec![
            ("id".to_owned(), "uuid/NO".to_owned()),
            ("doctype".to_owned(), "text/NO".to_owned()),
            ("label".to_owned(), "text/NO".to_owned()),
            ("owner_id".to_owned(), "uuid/YES".to_owned()),
            ("definition".to_owned(), "jsonb/NO".to_owned()),
            (
                "created_at".to_owned(),
                "timestamp with time zone/NO".to_owned()
            ),
            (
                "modified_at".to_owned(),
                "timestamp with time zone/NO".to_owned()
            ),
            ("version".to_owned(), "bigint/NO".to_owned()),
        ]
    );
    assert_eq!(
        db.rows(
            "SELECT indexname FROM pg_indexes WHERE tablename = 'fb_saved_view' \
             AND indexdef LIKE 'CREATE UNIQUE INDEX%' AND indexname <> 'fb_saved_view_pkey' \
             ORDER BY indexname"
        )
        .await,
        vec![
            vec!["fb_saved_view_private_label".to_owned()],
            vec!["fb_saved_view_shared_label".to_owned()]
        ]
    );
}
