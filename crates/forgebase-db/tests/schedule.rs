//! The scheduler (M6.2, ADR 0013) against a real PostgreSQL (see `common`): the sync of the
//! `fb_schedule` rows, the tick that enqueues due schedules exactly once however many workers
//! run, and the SQL of the built-in clean-up jobs.

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;

use common::TestDb;
use forgebase_core::meta::Registry;
use forgebase_db::schedule::{self, Declared, Spec};
use forgebase_db::{auth, connect_sized, jobs};
use time::{OffsetDateTime, Weekday};
use tokio::sync::Barrier;

const EVERY_5: Spec = Spec::EveryMinutes(5);
const DAILY_3: Spec = Spec::DailyAt { hour: 3, minute: 0 };

fn registry() -> Registry {
    let mut registry = Registry::new();
    assert!(registry.add_job("app.a"));
    assert!(registry.add_job("app.b"));
    registry
}

fn known() -> BTreeMap<String, Spec> {
    BTreeMap::from([("app.a".to_owned(), EVERY_5), ("app.b".to_owned(), DAILY_3)])
}

async fn sync(db: &TestDb, declared: &[(&str, Spec)], dry_run: bool) -> Vec<String> {
    let declared: Vec<Declared<'_>> = declared
        .iter()
        .map(|(name, spec)| Declared { name, spec })
        .collect();
    let mut client = db.pool.get().await.expect("connection");
    schedule::sync(&mut client, &declared, dry_run)
        .await
        .expect("sync")
}

async fn one(db: &TestDb, sql: &str) -> String {
    db.rows(sql).await.remove(0).remove(0)
}

#[tokio::test]
async fn sync_inserts_changes_keeps_and_deletes() {
    let db = TestDb::migrated().await;
    // Insert: due at the first time after the database's now().
    let lines = sync(&db, &[("app.a", EVERY_5), ("app.b", DAILY_3)], false).await;
    assert_eq!(lines, ["add app.a (every 5m)", "add app.b (daily 03:00)"]);
    assert_eq!(
        db.rows("SELECT name, spec, (next_run_at > now())::text, (last_run_at IS NULL)::text FROM fb_schedule ORDER BY name")
            .await,
        [
            ["app.a", "every 5m", "true", "true"],
            ["app.b", "daily 03:00", "true", "true"]
        ]
    );

    // Unchanged: left alone, `next_run_at` kept (set to a recognizable value first).
    db.exec("UPDATE fb_schedule SET next_run_at = '2030-01-01 00:00:00+00'")
        .await;
    assert!(
        sync(&db, &[("app.a", EVERY_5), ("app.b", DAILY_3)], false)
            .await
            .is_empty()
    );
    assert_eq!(
        one(
            &db,
            "SELECT count(*)::text FROM fb_schedule WHERE next_run_at = '2030-01-01 00:00:00+00'"
        )
        .await,
        "2"
    );

    // A changed spec is recomputed; the other row keeps its time; an undeclared row is deleted
    // (a dry run reports but keeps).
    let declared = [("app.a", Spec::EveryMinutes(10))];
    let planned = sync(&db, &declared, true).await;
    assert_eq!(
        planned,
        [
            "change app.a (every 5m -> every 10m)",
            "remove app.b (daily 03:00)"
        ]
    );
    assert_eq!(
        one(&db, "SELECT count(*)::text FROM fb_schedule").await,
        "2"
    );
    assert_eq!(
        one(&db, "SELECT spec FROM fb_schedule WHERE name = 'app.a'").await,
        "every 5m"
    );
    let lines = sync(&db, &declared, false).await;
    assert_eq!(lines, planned);
    assert_eq!(
        db.rows("SELECT name, spec, (next_run_at > now() AND next_run_at <= now() + interval '10 minutes')::text FROM fb_schedule").await,
        [["app.a", "every 10m", "true"]]
    );

    // Nothing declared: everything goes.
    assert_eq!(sync(&db, &[], false).await, ["remove app.a (every 10m)"]);
    assert_eq!(
        one(&db, "SELECT count(*)::text FROM fb_schedule").await,
        "0"
    );
    let weekly = Spec::WeeklyOn {
        weekday: Weekday::Monday,
        hour: 2,
        minute: 0,
    };
    assert_eq!(
        sync(&db, &[("app.w", weekly)], false).await,
        ["add app.w (weekly mon 02:00)"]
    );
}

#[tokio::test]
async fn a_tick_enqueues_a_due_schedule_with_the_due_time_and_the_scheduler_actor() {
    let db = TestDb::migrated().await;
    sync(&db, &[("app.a", EVERY_5), ("app.b", DAILY_3)], false).await;
    // Nothing is due yet.
    let mut client = db.pool.get().await.expect("connection");
    let registry = registry();
    assert_eq!(
        schedule::tick(
            &mut **client,
            &registry,
            &known(),
            Some(OffsetDateTime::UNIX_EPOCH)
        )
        .await
        .expect("tick"),
        0
    );

    db.exec("UPDATE fb_schedule SET next_run_at = '2026-01-01 00:05:00+00' WHERE name = 'app.a'")
        .await;
    assert_eq!(
        schedule::tick(&mut **client, &registry, &known(), None)
            .await
            .expect("tick"),
        1
    );
    assert_eq!(
        db.rows("SELECT name, payload->>'scheduled_for', enqueued_by, status FROM fb_job")
            .await,
        [["app.a", "2026-01-01T00:05:00Z", "scheduler", "queued"]]
    );
    // The schedule moved on to the future and records the run.
    assert_eq!(
        db.rows("SELECT (next_run_at > now())::text, (last_run_at IS NOT NULL)::text, ((extract(epoch FROM next_run_at)::bigint / 60) % 5)::text FROM fb_schedule WHERE name = 'app.a'").await,
        [["true", "true", "0"]]
    );
    // Ticking again enqueues nothing more.
    assert_eq!(
        schedule::tick(
            &mut **client,
            &registry,
            &known(),
            Some(OffsetDateTime::UNIX_EPOCH)
        )
        .await
        .expect("tick"),
        0
    );
    assert_eq!(one(&db, "SELECT count(*)::text FROM fb_job").await, "1");
}

#[tokio::test]
async fn missed_ticks_are_not_caught_up() {
    let db = TestDb::migrated().await;
    sync(&db, &[("app.a", EVERY_5)], false).await;
    // Ten periods in the past.
    db.exec(
        "UPDATE fb_schedule SET next_run_at = date_trunc('minute', now()) - interval '50 minutes'",
    )
    .await;
    let due = one(&db, "SELECT to_char(next_run_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') FROM fb_schedule").await;
    let mut client = db.pool.get().await.expect("connection");
    let known = BTreeMap::from([("app.a".to_owned(), EVERY_5)]);
    assert_eq!(
        schedule::tick(&mut **client, &registry(), &known, None)
            .await
            .expect("tick"),
        1
    );
    assert_eq!(
        schedule::tick(
            &mut **client,
            &registry(),
            &known,
            Some(OffsetDateTime::UNIX_EPOCH)
        )
        .await
        .expect("tick"),
        0
    );
    assert_eq!(
        db.rows("SELECT payload->>'scheduled_for' FROM fb_job")
            .await,
        [[due]]
    );
    assert_eq!(
        one(&db, "SELECT (next_run_at > now() AND next_run_at <= now() + interval '5 minutes')::text FROM fb_schedule").await,
        "true"
    );
}

#[tokio::test]
async fn a_tick_with_a_given_time_uses_it_instead_of_the_clock() {
    let db = TestDb::migrated().await;
    sync(&db, &[("app.a", EVERY_5)], false).await;
    let mut client = db.pool.get().await.expect("connection");
    let known = BTreeMap::from([("app.a".to_owned(), EVERY_5)]);
    let at = OffsetDateTime::now_utc() + time::Duration::days(1);
    assert_eq!(
        schedule::tick(&mut **client, &registry(), &known, Some(at))
            .await
            .expect("tick"),
        1
    );
    assert_eq!(
        one(
            &db,
            "SELECT (next_run_at > now() + interval '23 hours')::text FROM fb_schedule"
        )
        .await,
        "true"
    );
}

#[tokio::test]
async fn a_due_schedule_of_an_unknown_job_is_left_alone() {
    let db = TestDb::migrated().await;
    sync(&db, &[("app.a", EVERY_5), ("other.x", EVERY_5)], false).await;
    db.exec("UPDATE fb_schedule SET next_run_at = now() - interval '1 hour'")
        .await;
    let mut client = db.pool.get().await.expect("connection");
    let known = BTreeMap::from([("app.a".to_owned(), EVERY_5)]);
    assert_eq!(
        schedule::tick(&mut **client, &registry(), &known, None)
            .await
            .expect("tick"),
        1
    );
    assert_eq!(
        one(
            &db,
            "SELECT (next_run_at < now())::text FROM fb_schedule WHERE name = 'other.x'"
        )
        .await,
        "true"
    );
}

#[tokio::test]
async fn eight_concurrent_ticks_enqueue_each_due_time_exactly_once() {
    const WORKERS: usize = 8;
    const ROUNDS: usize = 50;
    let db = TestDb::migrated().await;
    sync(&db, &[("app.a", EVERY_5), ("app.b", DAILY_3)], false).await;
    let pool = connect_sized(&db.url(), Some(WORKERS + 2))
        .await
        .expect("pool");
    let registry = Arc::new(registry());
    let known = Arc::new(known());
    for round in 1..=ROUNDS {
        db.exec("UPDATE fb_schedule SET next_run_at = now() - interval '1 minute'")
            .await;
        // Every worker has its connection before any of them ticks, so the ticks overlap.
        let barrier = Arc::new(Barrier::new(WORKERS));
        let tasks: Vec<_> = (0..WORKERS)
            .map(|_| {
                let (pool, registry, known, barrier) = (
                    pool.clone(),
                    Arc::clone(&registry),
                    Arc::clone(&known),
                    Arc::clone(&barrier),
                );
                tokio::spawn(async move {
                    let mut client = pool.get().await.expect("connection");
                    barrier.wait().await;
                    schedule::tick(&mut **client, &registry, &known, None)
                        .await
                        .expect("tick")
                })
            })
            .collect();
        let mut enqueued = 0;
        for task in tasks {
            enqueued += task.await.expect("task");
        }
        assert_eq!(enqueued, 2, "round {round}: one job per schedule");
        assert_eq!(
            one(&db, "SELECT count(*)::text FROM fb_job").await,
            (2 * round).to_string(),
            "round {round}"
        );
    }
    assert_eq!(
        db.rows("SELECT name, count(*)::text FROM fb_job GROUP BY name ORDER BY name")
            .await,
        [["app.a", "50"], ["app.b", "50"]]
    );
}

/// Inserts a job row in a given state, finished `age` ago.
async fn job(db: &TestDb, status: &str, finished_ago: Option<&str>) {
    let finished = finished_ago.map_or("NULL".to_owned(), |a| format!("now() - interval '{a}'"));
    db.exec(&format!(
        "INSERT INTO fb_job (id, name, payload, status, enqueued_by, finished_at) \
         VALUES (gen_random_uuid(), 'app.a', '{{}}', '{status}', 't', {finished})"
    ))
    .await;
}

#[tokio::test]
async fn purge_finished_deletes_only_old_done_and_dead_jobs_in_batches() {
    let db = TestDb::migrated().await;
    job(&db, "done", Some("8 days")).await;
    job(&db, "done", Some("8 days")).await;
    job(&db, "dead", Some("8 days")).await;
    job(&db, "dead", Some("30 days")).await;
    job(&db, "done", Some("6 days")).await; // recent
    job(&db, "dead", Some("6 days")).await; // recent
    job(&db, "queued", None).await;
    job(&db, "running", None).await;
    // Old but not finished: never purged, whatever `created_at` or `run_at` say.
    db.exec("UPDATE fb_job SET created_at = now() - interval '90 days', run_at = now() - interval '90 days' WHERE status IN ('queued', 'running')")
        .await;
    let client = db.pool.get().await.expect("connection");
    assert_eq!(
        jobs::purge_finished(&**client, 3).await.expect("purge"),
        3,
        "a batch is bounded"
    );
    assert_eq!(jobs::purge_finished(&**client, 3).await.expect("purge"), 1);
    assert_eq!(jobs::purge_finished(&**client, 3).await.expect("purge"), 0);
    assert_eq!(
        db.rows("SELECT status, count(*)::text FROM fb_job GROUP BY status ORDER BY status")
            .await,
        [
            ["dead", "1"],
            ["done", "1"],
            ["queued", "1"],
            ["running", "1"]
        ]
    );
}

#[tokio::test]
async fn purge_finished_keeps_a_dead_job_that_was_retried() {
    let db = TestDb::migrated().await;
    job(&db, "dead", Some("30 days")).await;
    let id = db.rows("SELECT id::text FROM fb_job").await[0][0].clone();
    let client = db.pool.get().await.expect("connection");
    assert_eq!(
        jobs::retry_dead(&**client, &id).await.expect("retry"),
        jobs::RetryResult::Queued
    );
    assert_eq!(jobs::purge_finished(&**client, 10).await.expect("purge"), 0);
    assert_eq!(
        db.rows("SELECT status, (finished_at IS NULL)::text FROM fb_job")
            .await,
        [["queued", "true"]]
    );
}

#[tokio::test]
async fn purge_login_failures_keeps_rows_that_still_count_or_lock() {
    let db = TestDb::migrated().await;
    db.exec(
        "INSERT INTO fb_login_failure (username, failures, locked_until, last_failure_at) VALUES \
         ('stale', 3, NULL, now() - interval '16 minutes'), \
         ('stale_unlocked', 0, now() - interval '1 minute', now() - interval '20 minutes'), \
         ('never_failed', 0, NULL, NULL), \
         ('counting', 3, NULL, now() - interval '5 minutes'), \
         ('just_inside', 3, NULL, now() - interval '14 minutes 59 seconds'), \
         ('locked', 0, now() + interval '5 minutes', now() - interval '10 minutes'), \
         ('locked_old_failure', 0, now() + interval '1 minute', now() - interval '20 minutes')",
    )
    .await;
    let client = db.pool.get().await.expect("connection");
    assert_eq!(
        auth::purge_login_failures(&**client, 1000)
            .await
            .expect("purge"),
        3
    );
    assert_eq!(
        db.rows("SELECT username FROM fb_login_failure ORDER BY username")
            .await,
        [
            ["counting"],
            ["just_inside"],
            ["locked"],
            ["locked_old_failure"]
        ]
    );
    // A bounded batch.
    db.exec("UPDATE fb_login_failure SET locked_until = NULL, last_failure_at = now() - interval '1 hour'").await;
    assert_eq!(
        auth::purge_login_failures(&**client, 3)
            .await
            .expect("purge"),
        3
    );
    assert_eq!(
        auth::purge_login_failures(&**client, 3)
            .await
            .expect("purge"),
        1
    );
}

#[tokio::test]
async fn a_purged_login_failure_row_behaves_like_no_row() {
    // The five failures lock the user; after the lock and the window passed, a purge changes
    // nothing a login can see: the next failure counts as the first.
    let db = TestDb::migrated().await;
    {
        let mut client = db.pool.get().await.expect("connection");
        auth::create_user(&mut **client, "ann", "correct horse", &[])
            .await
            .expect("user");
        for _ in 0..5 {
            auth::login(&mut **client, "ann", "wrong password")
                .await
                .expect_err("refused");
        }
    }
    assert_eq!(
        one(
            &db,
            "SELECT (locked_until > now())::text FROM fb_login_failure"
        )
        .await,
        "true"
    );
    {
        let client = db.pool.get().await.expect("connection");
        assert_eq!(
            auth::purge_login_failures(&**client, 100)
                .await
                .expect("purge"),
            0,
            "locked"
        );
    }
    db.exec("UPDATE fb_login_failure SET locked_until = now() - interval '1 second', last_failure_at = now() - interval '16 minutes'").await;
    let mut client = db.pool.get().await.expect("connection");
    assert_eq!(
        auth::purge_login_failures(&**client, 100)
            .await
            .expect("purge"),
        1
    );
    auth::login(&mut **client, "ann", "correct horse")
        .await
        .expect("logs in");
}

#[tokio::test]
async fn purge_sessions_deletes_expired_sessions_only() {
    let db = TestDb::migrated().await;
    {
        let mut client = db.pool.get().await.expect("connection");
        auth::create_user(&mut **client, "ann", "correct horse", &[])
            .await
            .expect("user");
    }
    db.exec(
        "INSERT INTO fb_session (token_hash, csrf_hash, user_id, expires_at, last_seen_at) \
         SELECT sha256(convert_to(t, 'UTF8')), sha256(convert_to(t, 'UTF8')), u.id, e, s \
         FROM fb_user u, (VALUES \
           ('valid', now() + interval '1 day', now() - interval '1 hour'), \
           ('almost_idle', now() + interval '1 day', now() - interval '23 hours'), \
           ('absolute', now() - interval '1 second', now()), \
           ('idle', now() + interval '1 day', now() - interval '25 hours'), \
           ('both', now() - interval '1 day', now() - interval '3 days')) AS v(t, e, s)",
    )
    .await;
    let client = db.pool.get().await.expect("connection");
    assert_eq!(
        auth::purge_sessions(&**client, 2).await.expect("purge"),
        2,
        "bounded"
    );
    assert_eq!(auth::purge_sessions(&**client, 2).await.expect("purge"), 1);
    assert_eq!(auth::purge_sessions(&**client, 2).await.expect("purge"), 0);
    assert_eq!(
        db.rows(
            "SELECT t FROM unnest(ARRAY['valid', 'almost_idle', 'absolute', 'idle', 'both']) t \
             WHERE EXISTS (SELECT 1 FROM fb_session \
                           WHERE token_hash = sha256(convert_to(t, 'UTF8'))) ORDER BY t"
        )
        .await,
        [["almost_idle"], ["valid"]]
    );
}
