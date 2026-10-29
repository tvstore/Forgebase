//! The advisory lock of `migrate_apps` (M5.1): held for the whole migrate, so two runs never
//! interleave. The behaviour of the app migrations themselves is tested through the `forgebase`
//! crate (`crates/forgebase/tests/apps.rs`).

mod common;

use std::time::Duration;

use common::TestDb;
use forgebase_core::meta::Registry;
use forgebase_db::{AppMigration, AppSpec, DbError, SyncOptions, connect, migrate_apps};

/// The key of the migration lock (`MIGRATE_LOCK_KEY` in `migrate.rs`).
const KEY: i64 = 0x4642_4d49_4752_0001;

const SLOW: &[AppMigration<'static>] = &[AppMigration {
    version: 1,
    name: "slow",
    sql: "SELECT pg_sleep(0.5); CREATE TABLE zz_slow (n integer); INSERT INTO zz_slow VALUES (1)",
}];

const APP: AppSpec<'static> = AppSpec {
    name: "slow_app",
    version: Some("1"),
    migrations: SLOW,
};

#[tokio::test]
async fn two_concurrent_migrates_do_not_interleave() {
    let db = TestDb::new().await;
    let registry = Registry::new();
    let options = SyncOptions {
        dry_run: false,
        lock_timeout: Duration::from_secs(10),
    };
    let (first, second) = tokio::join!(
        migrate_apps(&db.pool, &registry, &[APP], &options),
        migrate_apps(&db.pool, &registry, &[APP], &options),
    );
    let (first, second) = (first.expect("first"), second.expect("second"));
    // One of them applied the migration, the other waited and found it applied.
    assert_eq!(first.app_migrations.len() + second.app_migrations.len(), 1);
    assert_eq!(
        db.rows("SELECT count(*)::text FROM zz_slow").await,
        vec![vec!["1".to_owned()]]
    );
}

#[tokio::test]
async fn a_held_migration_lock_makes_migrate_time_out_and_nothing_is_left_behind() {
    let db = TestDb::new().await;
    let registry = Registry::new();
    // Another session holds the lock, like a migrate that is still running.
    let holder = db.pool.get().await.expect("connection");
    holder
        .execute("SELECT pg_advisory_lock($1)", &[&KEY])
        .await
        .expect("lock");
    let options = SyncOptions {
        dry_run: false,
        lock_timeout: Duration::from_millis(200),
    };
    let error = tokio::time::timeout(
        Duration::from_secs(10),
        migrate_apps(&db.pool, &registry, &[APP], &options),
    )
    .await
    .expect("migrate_apps did not give up waiting for the lock")
    .expect_err("times out");
    assert!(matches!(error, DbError::LockTimeout), "{error:?}");
    assert_eq!(
        db.rows("SELECT coalesce(to_regclass('fb_migration')::text, 'none')")
            .await,
        vec![vec!["none".to_owned()]]
    );

    // Released: the migrate works, and releases its own lock again (the holder can take it).
    holder
        .execute("SELECT pg_advisory_unlock($1)", &[&KEY])
        .await
        .expect("unlock");
    let done = migrate_apps(&db.pool, &registry, &[APP], &SyncOptions::default())
        .await
        .expect("migrate");
    assert_eq!(done.app_migrations.len(), 1);
    let free = holder
        .query_one("SELECT pg_try_advisory_lock($1)", &[&KEY])
        .await
        .expect("try lock")
        .get::<_, bool>(0);
    assert!(free, "migrate_apps left the advisory lock held");
}

/// Advisory locks held in this test's database (the view is cluster-wide).
const HELD_LOCKS: &str = "SELECT count(*)::text FROM pg_locks WHERE locktype = 'advisory' \
    AND database = (SELECT oid FROM pg_database WHERE datname = current_database())";

#[tokio::test]
async fn a_cancelled_migrate_does_not_leave_its_lock_on_a_pooled_connection() {
    let db = TestDb::new().await;
    const SLEEPY: &[AppMigration<'static>] = &[AppMigration {
        version: 1,
        name: "sleepy",
        sql: "SELECT pg_sleep(1.5)",
    }];
    let pool = db.pool.clone();
    let task = tokio::spawn(async move {
        let app = AppSpec {
            name: "sleepy_app",
            version: None,
            migrations: SLEEPY,
        };
        migrate_apps(&pool, &Registry::new(), &[app], &SyncOptions::default()).await
    });
    // Wait until the migrate holds its lock, then drop it in the middle of the migration.
    let mut held = false;
    for _ in 0..100 {
        if db.rows(HELD_LOCKS).await == vec![vec!["1".to_owned()]] {
            held = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(held, "the migrate never took its lock");
    task.abort();
    assert!(task.await.expect_err("cancelled").is_cancelled());

    // The connection was closed, not returned to the pool alive: once the server notices
    // (when the running statement ends) the lock is gone.
    let mut free = false;
    for _ in 0..100 {
        if db.rows(HELD_LOCKS).await == vec![vec!["0".to_owned()]] {
            free = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(free, "a cancelled migrate left the advisory lock held");
}

#[tokio::test]
async fn an_app_migration_gives_up_waiting_for_a_lock_after_the_lock_timeout() {
    let db = TestDb::new().await;
    db.exec("CREATE TABLE zz_locked (n integer)").await;
    const BLOCKED: &[AppMigration<'static>] = &[AppMigration {
        version: 1,
        name: "blocked",
        sql: "INSERT INTO zz_locked VALUES (1); ALTER TABLE zz_locked ADD COLUMN extra text",
    }];
    let app = AppSpec {
        name: "blocked_app",
        version: None,
        migrations: BLOCKED,
    };
    // Another transaction holds a conflicting lock on the table and does not end.
    let mut holder = db.pool.get().await.expect("connection");
    let tx = holder.transaction().await.expect("transaction");
    tx.batch_execute("LOCK TABLE zz_locked IN ACCESS EXCLUSIVE MODE")
        .await
        .expect("lock the table");
    let options = SyncOptions {
        dry_run: false,
        lock_timeout: Duration::from_millis(300),
    };
    let started = std::time::Instant::now();
    // The timeout of the test itself: a regression (no lock timeout) fails fast instead of hanging.
    let error = tokio::time::timeout(
        Duration::from_secs(10),
        migrate_apps(&db.pool, &Registry::new(), &[app], &options),
    )
    .await
    .expect("migrate_apps did not give up waiting for the lock")
    .expect_err("times out");
    assert!(
        matches!(&error, DbError::AppMigrationFailed { version: 1, message, .. }
            if message.contains("timed out waiting for a database lock")),
        "{error:?}"
    );
    assert!(started.elapsed() < Duration::from_secs(5));
    tx.rollback().await.expect("rollback");
    // Nothing was recorded.
    assert_eq!(
        db.rows("SELECT count(*)::text FROM fb_app_migration").await,
        vec![vec!["0".to_owned()]]
    );
}

#[tokio::test]
async fn a_dry_run_with_pending_framework_migrations_lists_every_app_migration() {
    // Only the migrations before 0004 are applied: the app tables do not exist, so no app
    // migration can have been applied and all of them are pending; the sync plan is not known.
    let db = TestDb::migrated().await;
    db.exec("DROP TABLE fb_app, fb_app_migration; DELETE FROM fb_migration WHERE version = 4")
        .await;
    let client = db.pool.get().await.expect("connection");
    assert!(
        !forgebase_db::app_migration_tables_exist(&client)
            .await
            .expect("check")
    );
    drop(client);
    let options = SyncOptions {
        dry_run: true,
        lock_timeout: Duration::from_secs(5),
    };
    let report = migrate_apps(&db.pool, &Registry::new(), &[APP], &options)
        .await
        .expect("dry run");
    assert_eq!(report.migrations.len(), 1);
    assert!(report.sync.is_none());
    assert_eq!(report.app_migrations.len(), 1);
    // A dry run keeps nothing.
    assert_eq!(
        db.rows("SELECT (to_regclass('fb_app_migration') IS NULL)::text")
            .await,
        vec![vec!["true".to_owned()]]
    );
}

#[tokio::test]
async fn the_app_migration_tables_are_found_once_they_exist() {
    // The decision behind the dry run above: with the tables present (a later framework
    // migration pending) the app migrations are verified, not blindly listed.
    let db = TestDb::migrated().await;
    let client = db.pool.get().await.expect("connection");
    assert!(
        forgebase_db::app_migration_tables_exist(&client)
            .await
            .expect("check")
    );
}

const ONE: AppMigration<'static> = AppMigration {
    version: 1,
    name: "one",
    sql: "CREATE TABLE zz_one (n integer)",
};
const TWO: AppMigration<'static> = AppMigration {
    version: 2,
    name: "two",
    sql: "CREATE TABLE zz_two (n integer)",
};
/// `ONE` with other SQL: the same version and name, another checksum.
const ONE_EDITED: AppMigration<'static> = AppMigration {
    version: 1,
    name: "one",
    sql: "CREATE TABLE zz_one (n bigint)",
};

fn app<'a>(migrations: &'a [AppMigration<'a>]) -> AppSpec<'a> {
    AppSpec {
        name: "verify_app",
        version: Some("1"),
        migrations,
    }
}

#[tokio::test]
async fn a_dry_run_with_pending_framework_migrations_still_verifies_the_app_migrations() {
    // The situation of a later framework migration (0005, say) that is pending while the app
    // tables exist. None exists yet, so it is simulated: migration 0004 is made pending again.
    // A dry run executes the pending migration's SQL before rolling back, and 0004 would fail
    // on tables that exist; so the real tables are moved to the schema `scratch`, which comes
    // after `public` in the search path: 0004 creates its tables in `public` (and rolls them
    // back), and the app migration queries still find the moved ones.
    let db = TestDb::migrated().await;
    let options = SyncOptions {
        dry_run: false,
        lock_timeout: Duration::from_secs(5),
    };
    let registry = Registry::new();
    migrate_apps(&db.pool, &registry, &[app(&[ONE])], &options)
        .await
        .expect("apply the first app migration");
    db.exec(
        "CREATE SCHEMA scratch; \
         ALTER TABLE fb_app SET SCHEMA scratch; \
         ALTER TABLE fb_app_migration SET SCHEMA scratch; \
         DO $$ BEGIN EXECUTE format('ALTER DATABASE %I SET search_path = public, scratch', \
             current_database()); END $$; \
         DELETE FROM fb_migration WHERE version = 4",
    )
    .await;
    // New connections have the search path; the pool of `db` may still hold old ones.
    let pool = connect(&db.url()).await.expect("pool");
    let dry = SyncOptions {
        dry_run: true,
        lock_timeout: Duration::from_secs(5),
    };

    // An applied migration whose SQL changed is found, though framework migrations are pending.
    let error = migrate_apps(&pool, &registry, &[app(&[ONE_EDITED])], &dry)
        .await
        .expect_err("the changed migration is refused");
    assert!(
        matches!(&error, DbError::AppChecksumMismatch { version: 1, .. }),
        "{error:?}"
    );

    // Only the migrations that are really pending are listed, not every declared one.
    let report = migrate_apps(&pool, &registry, &[app(&[ONE, TWO])], &dry)
        .await
        .expect("dry run");
    assert_eq!(report.migrations.len(), 1, "the framework migration 0004");
    assert!(report.sync.is_none());
    assert_eq!(
        report.app_migrations.len(),
        1,
        "{:?}",
        report.app_migrations
    );
    assert_eq!(report.app_migrations[0].version, 2);
    assert_eq!(report.app_migrations[0].name, "two");
}
