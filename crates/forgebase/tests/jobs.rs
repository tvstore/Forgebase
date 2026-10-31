//! Background jobs through the public API (M6.1, ADR 0013), with `forgebase::testing`: the
//! outbox from a hook over HTTP, delivery, retry and dead jobs, the documents handle, jobs that
//! enqueue jobs, and the startup errors. The worker's own behaviour (leases, shutdown,
//! concurrency) is tested next to its code, in `src/worker.rs`.
//!
//! Each test gets its own throwaway database and fails loudly (never skips) when
//! `FORGEBASE_TEST_DATABASE_URL` is not set.

use std::time::Duration;

use forgebase::serde_json::{Value as Json, json};
use forgebase::testing::{TestDb, run_jobs};
use forgebase::{
    App, Changes, DocStatus, DocTypeFile, DocTypeHooks, Document, FilterOp, Forgebase, HookContext,
    HookError, JobContext, JobError, JobFuture, JobHandler, JobOptions, ListQuery, Value,
};

const NOTE: &str = r#"
name = "note"
label = "Note"
[[fields]]
name = "title"
type = "data"
required = true
[[fields]]
name = "qty"
type = "int"
"#;

const INVOICE: &str = r#"
name = "invoice"
label = "Invoice"
submittable = true
[[fields]]
name = "title"
type = "data"
required = true
"#;

const TICKET: &str = r#"
name = "ticket"
label = "Ticket"
[[fields]]
name = "title"
type = "data"
required = true

[[permissions]]
role = "Agent"
read = true
write = true
create = true
"#;

fn text(s: &str) -> Value {
    Value::Text(s.to_owned())
}

fn note(title: &str) -> Document {
    Document {
        values: [("title".to_owned(), text(title))].into(),
        ..Document::default()
    }
}

/// What a test job does. One handler type with a behaviour keeps the tests short.
#[derive(Clone)]
enum Behavior {
    /// Insert a note titled after the payload's `title`, then succeed.
    InsertNote,
    /// Insert a note, then fail with a retryable error.
    InsertThenFail,
    /// Fail with a permanent error.
    Permanent,
    /// Insert a note without its required title.
    InsertInvalid,
    /// Panic.
    Panic,
    /// Insert a note, then sleep for a long time.
    InsertThenSleep,
    /// Enqueue this job with the payload `{}`, then succeed.
    Enqueue(&'static str),
    /// Enqueue this job, then fail.
    EnqueueThenFail(&'static str),
    /// Use every operation of the documents handle on an invoice.
    Tour,
}

struct Script(Behavior);

impl JobHandler for Script {
    fn run<'a>(&'a self, ctx: &'a mut JobContext<'_>) -> JobFuture<'a> {
        Box::pin(async move {
            match &self.0 {
                Behavior::InsertNote => {
                    let title = ctx.payload()["title"]
                        .as_str()
                        .unwrap_or("untitled")
                        .to_owned();
                    ctx.documents().insert("note", note(&title)).await?;
                    Ok(())
                }
                Behavior::InsertThenFail => {
                    ctx.documents().insert("note", note("rolled back")).await?;
                    Err(JobError::new("boom"))
                }
                Behavior::Permanent => Err(JobError::permanent("never works")),
                Behavior::InsertInvalid => {
                    ctx.documents().insert("note", Document::default()).await?;
                    Ok(())
                }
                Behavior::Panic => panic!("handler bug"),
                Behavior::InsertThenSleep => {
                    ctx.documents().insert("note", note("slow")).await?;
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    Ok(())
                }
                Behavior::Enqueue(name) => ctx.enqueue(name, json!({"title": "child"})).await,
                Behavior::EnqueueThenFail(name) => {
                    ctx.enqueue(name, json!({"title": "child"})).await?;
                    Err(JobError::new("after enqueue"))
                }
                Behavior::Tour => tour(ctx).await,
            }
        })
    }
}

async fn tour(ctx: &mut JobContext<'_>) -> Result<(), JobError> {
    let mut docs = ctx.documents();
    let invoice = Document {
        values: [("title".to_owned(), text("inv"))].into(),
        ..Document::default()
    };
    let stored = docs.insert("invoice", invoice).await?;
    assert_eq!(stored.version(), 1);
    assert_eq!(stored.docstatus(), Some(DocStatus::Draft));
    let name = stored.name().to_owned();

    let got = docs.get("invoice", &name).await?.expect("found");
    assert_eq!(got.document().values["title"], text("inv"));
    assert!(docs.get("invoice", "nobody").await?.is_none());

    let changes = Changes {
        values: [("title".to_owned(), text("inv 2"))].into(),
        ..Changes::default()
    };
    let updated = docs.update("invoice", &name, 1, changes).await?;
    assert_eq!(updated.version(), 2);

    // A stale version is a permanent error, and the job's transaction stays usable.
    let stale = Changes {
        values: [("title".to_owned(), text("x"))].into(),
        ..Changes::default()
    };
    let error = docs
        .update("invoice", &name, 1, stale)
        .await
        .expect_err("conflict");
    assert!(!error.is_retryable(), "{error}");

    let found = docs
        .list(
            "invoice",
            &ListQuery::new()
                .filter("title", FilterOp::Like, text("inv%"))
                .order_by("name", true)
                .limit(5),
        )
        .await?;
    assert_eq!(found.len(), 1);
    let bad = ListQuery::new().filter("title", FilterOp::Like, Value::Int(1));
    assert!(
        !docs
            .list("invoice", &bad)
            .await
            .expect_err("text only")
            .is_retryable()
    );

    let submitted = docs.submit("invoice", &name, 2).await?;
    assert_eq!(submitted.docstatus(), Some(DocStatus::Submitted));
    let cancelled = docs.cancel("invoice", &name, 3).await?;
    assert_eq!(cancelled.docstatus(), Some(DocStatus::Cancelled));
    let amended = docs.amend("invoice", &name, 4).await?;
    assert_eq!(amended.docstatus(), Some(DocStatus::Draft));
    docs.delete("invoice", amended.name(), amended.version())
        .await?;
    Ok(())
}

/// A hook that asks for `support.notify` for every ticket; the title `bad` asks for a job that
/// does not exist.
struct TicketHooks;

impl DocTypeHooks for TicketHooks {
    fn before_insert(&self, ctx: &HookContext<'_>, doc: &mut Document) -> Result<(), HookError> {
        let title = match doc.values.get("title") {
            Some(Value::Text(title)) => title.clone(),
            _ => return Ok(()),
        };
        let job = if title == "bad" {
            "support.nobody"
        } else {
            "support.notify"
        };
        ctx.enqueue(job, json!({"title": title}))
    }
}

const FILES: &[DocTypeFile] = &[
    DocTypeFile::new("doctypes/note.toml", NOTE),
    DocTypeFile::new("doctypes/invoice.toml", INVOICE),
    DocTypeFile::new("doctypes/ticket.toml", TICKET),
];

fn support(jobs: Vec<(&'static str, Behavior, JobOptions)>) -> Forgebase {
    let mut app = App::new("support")
        .doctypes(FILES)
        .hooks("ticket", TicketHooks);
    for (name, behavior, options) in jobs {
        app = app.job_with(name, Script(behavior), options);
    }
    Forgebase::new().app(app)
}

fn quick() -> JobOptions {
    JobOptions::new()
}

async fn ready(binary: &Forgebase) -> TestDb {
    let db = TestDb::new().await;
    db.migrate(binary, false).await.expect("migrate");
    db
}

async fn enqueue(db: &TestDb, name: &str, payload: &str) {
    db.exec(&format!(
        "INSERT INTO fb_job (id, name, payload, status, enqueued_by) \
         VALUES (gen_random_uuid(), '{name}', '{payload}', 'queued', 'test')"
    ))
    .await;
}

async fn job_row(db: &TestDb) -> Vec<String> {
    let mut rows = db
        .rows("SELECT status, attempts::text, coalesce(last_error, '-') FROM fb_job ORDER BY id")
        .await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    rows.remove(0)
}

async fn note_titles(db: &TestDb) -> Vec<String> {
    db.rows("SELECT title FROM doc_note ORDER BY title")
        .await
        .into_iter()
        .map(|mut r| r.remove(0))
        .collect()
}

// ---- the outbox, over HTTP ------------------------------------------------------------------

#[tokio::test]
async fn a_hook_enqueues_over_http_and_the_job_runs_with_its_documents() {
    let binary = support(vec![("notify", Behavior::InsertNote, quick())]);
    let db = ready(&binary).await;
    db.create_user("ann", &["Agent"]).await;
    let ann = db.client(&binary).login("ann").await;

    let reply = ann
        .post("/api/v1/resource/ticket", json!({"title": "printer"}))
        .await;
    assert_eq!(reply.status, 201, "{:?}", reply.body);
    assert_eq!(
        db.rows("SELECT name, status, enqueued_by, payload::text FROM fb_job")
            .await,
        [[
            "support.notify",
            "queued",
            "ann",
            "{\"title\": \"printer\"}"
        ]]
    );

    let run = run_jobs(&binary, &db).await;
    assert_eq!((run.done(), run.retried(), run.dead()), (1, 0, 0));
    assert_eq!(note_titles(&db).await, ["printer"]);
    // The handler's insert is attributed to the job.
    assert_eq!(
        db.rows("SELECT owner, modified_by FROM doc_note").await,
        [["job:support.notify", "job:support.notify"]]
    );
    assert_eq!(job_row(&db).await, ["done", "1", "-"]);
    db.cleanup().await;
}

#[tokio::test]
async fn a_hook_that_enqueues_an_unknown_job_fails_the_request_and_writes_nothing() {
    let binary = support(vec![("notify", Behavior::InsertNote, quick())]);
    let db = ready(&binary).await;
    db.create_user("ann", &["Agent"]).await;
    let ann = db.client(&binary).login("ann").await;
    let reply = ann
        .post("/api/v1/resource/ticket", json!({"title": "bad"}))
        .await;
    assert_eq!(reply.status, 500, "{:?}", reply.body);
    assert_eq!(reply.body["error"]["code"], "internal");
    assert_eq!(
        db.rows("SELECT count(*)::text FROM doc_ticket").await,
        [["0"]]
    );
    assert_eq!(db.rows("SELECT count(*)::text FROM fb_job").await, [["0"]]);
    db.cleanup().await;
}

// ---- delivery -------------------------------------------------------------------------------

#[tokio::test]
async fn a_failed_attempt_rolls_back_and_retries_after_the_backoff() {
    let binary = support(vec![("flaky", Behavior::InsertThenFail, quick())]);
    let db = ready(&binary).await;
    enqueue(&db, "support.flaky", "{}").await;

    let run = run_jobs(&binary, &db).await;
    assert_eq!((run.done(), run.retried(), run.dead()), (0, 1, 0));
    assert!(
        note_titles(&db).await.is_empty(),
        "the handler's insert was rolled back"
    );
    assert_eq!(job_row(&db).await, ["queued", "1", "boom"]);
    // The default backoff is 30 s; the clock is the database's.
    assert_eq!(
        db.rows(
            "SELECT (run_at BETWEEN now() + interval '25 seconds' AND now() + interval '35 seconds')::text FROM fb_job"
        )
        .await,
        [["true"]]
    );
    // It is not due, so another call runs nothing.
    assert_eq!(run_jobs(&binary, &db).await.retried(), 0);
    db.cleanup().await;
}

#[tokio::test]
async fn a_job_is_dead_after_its_attempts_and_a_permanent_error_after_one() {
    let options = JobOptions::new()
        .max_attempts(2)
        .backoff(Duration::from_secs(3600), Duration::from_secs(3600));
    let binary = support(vec![
        ("flaky", Behavior::InsertThenFail, options),
        ("never", Behavior::Permanent, quick()),
    ]);
    let db = ready(&binary).await;
    enqueue(&db, "support.flaky", "{}").await;
    let run = run_jobs(&binary, &db).await;
    assert_eq!((run.retried(), run.dead()), (1, 0));
    db.exec("UPDATE fb_job SET run_at = now()").await;
    let run = run_jobs(&binary, &db).await;
    assert_eq!((run.retried(), run.dead()), (0, 1));
    assert_eq!(job_row(&db).await, ["dead", "2", "boom"]);

    db.exec("DELETE FROM fb_job").await;
    enqueue(&db, "support.never", "{}").await;
    let run = run_jobs(&binary, &db).await;
    assert_eq!((run.retried(), run.dead()), (0, 1));
    assert_eq!(job_row(&db).await, ["dead", "1", "never works"]);
    db.cleanup().await;
}

#[tokio::test]
async fn a_validation_error_of_the_documents_handle_is_permanent() {
    let binary = support(vec![("invalid", Behavior::InsertInvalid, quick())]);
    let db = ready(&binary).await;
    enqueue(&db, "support.invalid", "{}").await;
    let run = run_jobs(&binary, &db).await;
    assert_eq!((run.retried(), run.dead()), (0, 1));
    let row = job_row(&db).await;
    assert_eq!(row[..2], ["dead", "1"]);
    assert!(row[2].contains("validation failed"), "{row:?}");
    db.cleanup().await;
}

#[tokio::test]
async fn a_panic_is_a_retryable_failure_and_the_run_goes_on() {
    let binary = support(vec![
        ("crash", Behavior::Panic, quick()),
        ("notify", Behavior::InsertNote, quick()),
    ]);
    let db = ready(&binary).await;
    enqueue(&db, "support.crash", "{}").await;
    enqueue(&db, "support.notify", "{\"title\": \"after\"}").await;
    let run = run_jobs(&binary, &db).await;
    assert_eq!((run.done(), run.retried(), run.dead()), (1, 1, 0));
    let rows = db
        .rows("SELECT status, last_error FROM fb_job WHERE name = 'support.crash'")
        .await;
    assert_eq!(rows, [["queued", "panicked: handler bug"]]);
    assert_eq!(note_titles(&db).await, ["after"]);
    db.cleanup().await;
}

#[tokio::test]
async fn a_timeout_fails_the_attempt_and_rolls_back_its_effects() {
    let options = JobOptions::new().timeout(Duration::from_millis(300));
    let binary = support(vec![("slow", Behavior::InsertThenSleep, options)]);
    let db = ready(&binary).await;
    enqueue(&db, "support.slow", "{}").await;
    let run = run_jobs(&binary, &db).await;
    assert_eq!((run.done(), run.retried(), run.dead()), (0, 1, 0));
    assert!(note_titles(&db).await.is_empty());
    let row = job_row(&db).await;
    assert_eq!(row[..2], ["queued", "1"]);
    assert!(row[2].starts_with("timed out after"), "{row:?}");
    db.cleanup().await;
}

#[tokio::test]
async fn a_worker_leaves_jobs_it_does_not_know_alone() {
    let binary = support(vec![("notify", Behavior::InsertNote, quick())]);
    let db = ready(&binary).await;
    enqueue(&db, "elsewhere.thing", "{}").await;
    let run = run_jobs(&binary, &db).await;
    assert_eq!((run.done(), run.retried(), run.dead()), (0, 0, 0));
    assert_eq!(job_row(&db).await, ["queued", "0", "-"]);
    db.cleanup().await;
}

// ---- jobs that enqueue jobs and use documents ------------------------------------------------

#[tokio::test]
async fn a_job_enqueues_a_job_in_its_own_transaction() {
    let binary = support(vec![
        ("parent", Behavior::Enqueue("support.child"), quick()),
        ("child", Behavior::InsertNote, quick()),
        (
            "failing",
            Behavior::EnqueueThenFail("support.child"),
            quick(),
        ),
        ("unknown", Behavior::Enqueue("support.nobody"), quick()),
    ]);
    let db = ready(&binary).await;

    // The parent completes: the child exists and runs in the same call.
    enqueue(&db, "support.parent", "{}").await;
    let run = run_jobs(&binary, &db).await;
    assert_eq!(run.done(), 2);
    assert_eq!(note_titles(&db).await, ["child"]);
    assert_eq!(
        db.rows("SELECT enqueued_by FROM fb_job WHERE name = 'support.child'")
            .await,
        [["job:support.parent"]]
    );

    // A job that fails after enqueueing leaves no child behind.
    db.exec("DELETE FROM fb_job; DELETE FROM doc_note").await;
    enqueue(&db, "support.failing", "{}").await;
    let run = run_jobs(&binary, &db).await;
    assert_eq!((run.done(), run.retried()), (0, 1));
    assert_eq!(db.rows("SELECT count(*)::text FROM fb_job").await, [["1"]]);

    // An unknown name is a permanent error.
    db.exec("DELETE FROM fb_job").await;
    enqueue(&db, "support.unknown", "{}").await;
    let run = run_jobs(&binary, &db).await;
    assert_eq!((run.retried(), run.dead()), (0, 1));
    assert!(job_row(&db).await[2].contains("support.nobody"));
    db.cleanup().await;
}

#[tokio::test]
async fn the_documents_handle_does_every_operation() {
    let binary = support(vec![("tour", Behavior::Tour, quick())]);
    let db = ready(&binary).await;
    enqueue(&db, "support.tour", "{}").await;
    let run = run_jobs(&binary, &db).await;
    assert_eq!(
        (run.done(), run.retried(), run.dead()),
        (1, 0, 0),
        "{:?}",
        job_row(&db).await
    );
    // The cancelled original is left; the amendment was deleted.
    assert_eq!(
        db.rows("SELECT docstatus::text, owner FROM doc_invoice")
            .await,
        [["2", "job:support.tour"]]
    );
    db.cleanup().await;
}

// ---- startup errors --------------------------------------------------------------------------

struct Nothing;

impl JobHandler for Nothing {
    fn run<'a>(&'a self, _: &'a mut JobContext<'_>) -> JobFuture<'a> {
        Box::pin(async { Ok(()) })
    }
}

#[test]
fn job_mistakes_are_startup_errors() {
    let bad_options = JobOptions::new()
        .max_attempts(0)
        .timeout(Duration::ZERO)
        .backoff(Duration::from_secs(9), Duration::from_secs(1));
    let app = App::new("shop")
        .job("Bad-Name", Nothing)
        .job("fine", Nothing)
        .job("fine", Nothing)
        .job_with("tuned", Nothing, bad_options);
    let error = Forgebase::new().app(app).check().expect_err("refused");
    let problems: Vec<&str> = error.problems().collect();
    assert_eq!(problems.len(), 5, "{problems:?}");
    assert!(
        problems[0].contains("job name \"Bad-Name\""),
        "{problems:?}"
    );
    assert!(problems[1].contains("job `fine` is registered more than once"));
    assert!(problems[2].contains("job `tuned`: max_attempts must be at least 1"));
    assert!(problems[3].contains("timeout must be more than zero"));
    assert!(problems[4].contains("backoff cap"));
    assert!(
        Forgebase::new()
            .app(App::new("shop").job("fine", Nothing))
            .check()
            .is_ok(),
        "the same name in another app is fine"
    );
    let two = Forgebase::new()
        .app(App::new("a").job("fine", Nothing))
        .app(App::new("b").job("fine", Nothing));
    assert!(two.check().is_ok());
}

#[test]
fn a_hook_context_made_by_hand_cannot_enqueue_until_it_has_a_collector() {
    use forgebase::{EnqueuedJobs, Operation};
    let ctx = HookContext::new("ticket", "ann", Operation::Insert);
    assert!(ctx.enqueue("support.notify", Json::Null).is_err());
    let jobs = EnqueuedJobs::new();
    let ctx = ctx.with_jobs(&jobs);
    let mut doc = Document::default();
    doc.values.insert("title".to_owned(), text("printer"));
    TicketHooks.before_insert(&ctx, &mut doc).expect("enqueued");
    let taken = jobs.take();
    assert_eq!(taken.len(), 1);
    assert_eq!(taken[0].name(), "support.notify");
    assert_eq!(taken[0].payload(), &json!({"title": "printer"}));
}
