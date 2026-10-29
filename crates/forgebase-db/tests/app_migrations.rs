//! The advisory lock of `migrate_apps` (M5.1): held for the whole migrate, so two runs never
//! interleave. The behaviour of the app migrations themselves is tested through the `forgebase`
//! crate (`crates/forgebase/tests/apps.rs`).

mod common;

use std::time::Duration;

use common::TestDb;
use forgebase_core::meta::Registry;
use forgebase_db::{AppMigration, AppSpec, DbError, SyncOptions, migrate_apps};

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
    let error = migrate_apps(&db.pool, &registry, &[APP], &options)
        .await
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
    let error = migrate_apps(&db.pool, &Registry::new(), &[app], &options)
        .await
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
