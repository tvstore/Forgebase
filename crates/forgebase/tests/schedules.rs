//! Schedules through the public API (M6.2, ADR 0013), with `forgebase::testing`: the startup
//! errors, what `migrate` stores, and `run_schedules_at` followed by `run_jobs`. The tick itself
//! (exactly once under concurrency, missed ticks) is tested in `forgebase-db`, and the worker
//! that ticks in `src/worker.rs`.
//!
//! Each test gets its own throwaway database and fails loudly (never skips) when
//! `FORGEBASE_TEST_DATABASE_URL` is not set.

use forgebase::testing::{TestDb, run_jobs, run_schedules_at};
use forgebase::{
    App, DocTypeFile, Document, Forgebase, JobContext, JobFuture, JobHandler, OffsetDateTime,
    Schedule, Value, Weekday,
};

const SEEN: &str =
    "name = \"seen\"\nlabel = \"Seen\"\n[[fields]]\nname = \"at\"\ntype = \"data\"\n";

/// Records the `scheduled_for` of its payload as a `seen` document.
struct Record;

impl JobHandler for Record {
    fn run<'a>(&'a self, ctx: &'a mut JobContext<'_>) -> JobFuture<'a> {
        Box::pin(async move {
            let at = ctx.payload()["scheduled_for"]
                .as_str()
                .unwrap_or("none")
                .to_owned();
            let document = Document {
                values: [("at".to_owned(), Value::Text(at))].into(),
                ..Document::default()
            };
            ctx.documents().insert("seen", document).await?;
            Ok(())
        })
    }
}

fn crm(schedule: Option<Schedule>) -> Forgebase {
    let app = App::new("crm")
        .doctypes(const { &[DocTypeFile::new("seen.toml", SEEN)] })
        .job("digest", Record);
    let app = match schedule {
        Some(schedule) => app.schedule("digest", schedule),
        None => app,
    };
    Forgebase::new().app(app)
}

async fn first(db: &TestDb, sql: &str) -> String {
    db.rows(sql).await.remove(0).remove(0)
}

// ---- startup errors --------------------------------------------------------------------------

#[test]
fn schedule_mistakes_are_startup_errors() {
    let app = App::new("shop")
        .job("fine", Record)
        .job("other", Record)
        .schedule("fine", Schedule::every_minutes(0))
        .schedule("fine", Schedule::daily_at(1, 0)) // a second schedule for the job
        .schedule("other", Schedule::daily_at(24, 60)) // two problems
        .schedule("ghost", Schedule::every_minutes(5)) // not registered
        .schedule("nope", Schedule::weekly_on(Weekday::Monday, 0, 60));
    let error = Forgebase::new().app(app).check().expect_err("refused");
    let problems: Vec<&str> = error.problems().collect();
    assert_eq!(problems.len(), 6, "{problems:?}");
    assert!(
        problems[0].contains("schedule for job `fine`") && problems[0].contains("from 1 to 1440"),
        "{problems:?}"
    );
    assert!(problems[1].contains("job `fine` has more than one schedule"));
    assert!(problems[2].contains("the hour 24"));
    assert!(problems[3].contains("the minute 60"));
    assert!(problems[4].contains("schedule for job `ghost`, which `shop` did not register"));
    assert!(problems[5].contains("schedule for job `nope`"));
    assert!(error.to_string().starts_with("invalid app setup:"));
}

#[test]
fn an_app_cannot_schedule_the_job_of_another_app() {
    let a = App::new("a").job("work", Record);
    let b = App::new("b").schedule("a.work", Schedule::every_minutes(5));
    let error = Forgebase::new().app(a).app(b).check().expect_err("refused");
    let text = error.to_string();
    assert!(
        text.contains("schedule for job `a.work`, which `b` did not register"),
        "{text}"
    );
}

#[test]
fn the_edges_of_every_kind_are_valid() {
    let app = App::new("shop")
        .job("a", Record)
        .job("b", Record)
        .job("c", Record)
        .job("d", Record)
        .schedule("a", Schedule::every_minutes(1))
        .schedule("b", Schedule::every_minutes(1440))
        .schedule("c", Schedule::daily_at(23, 59))
        .schedule("d", Schedule::weekly_on(Weekday::Sunday, 0, 0));
    assert!(Forgebase::new().app(app).check().is_ok());
}

#[test]
fn the_app_name_forgebase_is_reserved_for_the_built_in_jobs() {
    let error = Forgebase::new()
        .app(App::new("forgebase").job("purge_jobs", Record))
        .check()
        .expect_err("refused");
    let text = error.to_string();
    assert!(
        text.contains("app name `forgebase` is reserved for the framework's own jobs"),
        "{text}"
    );
}

#[test]
fn schedules_compare_by_value() {
    assert_eq!(
        Schedule::every_minutes(5),
        Schedule::every_minutes(5).clone()
    );
    assert_ne!(Schedule::every_minutes(5), Schedule::every_minutes(6));
    assert_ne!(
        Schedule::daily_at(1, 0),
        Schedule::weekly_on(Weekday::Monday, 1, 0)
    );
}

// ---- migrate stores the schedules ------------------------------------------------------------

#[tokio::test]
async fn migrate_stores_the_built_in_and_app_schedules_and_keeps_them_in_step() {
    let db = TestDb::new().await;
    let binary = crm(Some(Schedule::every_minutes(15)));
    db.migrate(&binary, false).await.expect("migrate");
    assert_eq!(
        db.rows("SELECT name, spec FROM fb_schedule ORDER BY name")
            .await,
        [
            ["crm.digest", "every 15m"],
            ["forgebase.purge_jobs", "daily 03:00"],
            ["forgebase.purge_login_failures", "every 60m"],
            ["forgebase.purge_sessions", "every 60m"],
        ]
    );

    // An unchanged schedule keeps its `next_run_at` over another migrate.
    db.exec("UPDATE fb_schedule SET next_run_at = '2031-01-01 00:00:00+00'")
        .await;
    db.migrate(&binary, false).await.expect("migrate again");
    assert_eq!(
        first(
            &db,
            "SELECT count(*)::text FROM fb_schedule WHERE next_run_at = '2031-01-01 00:00:00+00'"
        )
        .await,
        "4"
    );

    // A changed one is recomputed, and the others stay.
    let changed = crm(Some(Schedule::daily_at(6, 30)));
    db.migrate(&changed, false).await.expect("migrate changed");
    assert_eq!(
        db.rows("SELECT spec, (next_run_at <> '2031-01-01 00:00:00+00')::text FROM fb_schedule WHERE name = 'crm.digest'").await,
        [["daily 06:30", "true"]]
    );
    assert_eq!(
        first(
            &db,
            "SELECT count(*)::text FROM fb_schedule WHERE next_run_at = '2031-01-01 00:00:00+00'"
        )
        .await,
        "3"
    );

    // A binary that does not declare the schedule deletes the row; the built-ins stay.
    db.migrate(&crm(None), false)
        .await
        .expect("migrate without");
    assert_eq!(
        first(
            &db,
            "SELECT string_agg(name, ',' ORDER BY name) FROM fb_schedule"
        )
        .await,
        "forgebase.purge_jobs,forgebase.purge_login_failures,forgebase.purge_sessions"
    );
    db.cleanup().await;
}

// ---- run_schedules_at and run_jobs -----------------------------------------------------------

#[tokio::test]
async fn run_schedules_at_enqueues_the_due_jobs_and_run_jobs_runs_them() {
    let db = TestDb::new().await;
    let binary = crm(Some(Schedule::every_minutes(15)));
    db.migrate(&binary, false).await.expect("migrate");

    // Nothing is due an hour ago (every schedule was set after the migrate): nothing is enqueued.
    let past = OffsetDateTime::now_utc() - std::time::Duration::from_secs(3600);
    assert_eq!(run_schedules_at(&binary, &db, past).await, 0);

    // Two days later every schedule is due: the app's and the three built-in ones.
    let due = first(
        &db,
        "SELECT to_char(next_run_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') \
         FROM fb_schedule WHERE name = 'crm.digest'",
    )
    .await;
    let at = OffsetDateTime::now_utc() + std::time::Duration::from_secs(2 * 24 * 3600);
    assert_eq!(run_schedules_at(&binary, &db, at).await, 4);
    assert_eq!(run_schedules_at(&binary, &db, at).await, 0, "enqueued once");
    assert_eq!(
        db.rows(
            "SELECT payload->>'scheduled_for', enqueued_by FROM fb_job WHERE name = 'crm.digest'"
        )
        .await,
        [[due.as_str(), "scheduler"]]
    );

    let run = run_jobs(&binary, &db).await;
    assert_eq!((run.done(), run.retried(), run.dead()), (4, 0, 0));
    assert_eq!(
        db.rows("SELECT \"at\" FROM doc_seen").await,
        [[due.as_str()]],
        "the handler saw the due time"
    );

    // Later still, it is due again: the next tick is a new job.
    let later = at + std::time::Duration::from_secs(3600);
    assert!(run_schedules_at(&binary, &db, later).await >= 1);
    db.cleanup().await;
}

#[tokio::test]
async fn built_in_jobs_run_on_a_binary_without_apps() {
    let db = TestDb::new().await;
    let binary = Forgebase::new();
    db.migrate(&binary, false).await.expect("migrate");
    let at = OffsetDateTime::now_utc() + std::time::Duration::from_secs(2 * 24 * 3600);
    assert_eq!(run_schedules_at(&binary, &db, at).await, 3);
    let run = run_jobs(&binary, &db).await;
    assert_eq!((run.done(), run.dead()), (3, 0));
    db.cleanup().await;
}
