//! The job queue and the outbox (M6.1) against a real PostgreSQL (see `common`): jobs that
//! hooks enqueue are written in the operation's transaction, and the queue functions claim,
//! complete, fail, release and retry rows with the guards and leases of ADR 0013.

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use common::{TestDb, registry};
use forgebase_core::document::{Changes, Document};
use forgebase_core::hooks::{DocTypeHooks, HookContext, HookError, MAX_JOB_PAYLOAD_BYTES};
use forgebase_core::meta::Registry;
use forgebase_core::permission::Caller;
use forgebase_core::value::Value;
use forgebase_db::jobs::{self, Failure, JobFilter, Known, RetryResult};
use forgebase_db::{DbError, amend, cancel, delete, insert, submit, update};
use serde_json::json;

const QUOTE: &str = r#"
name = "quote"
label = "Quote"
submittable = true
[[fields]]
name = "title"
type = "data"
required = true
"#;

/// A DocType whose title is unique, to make a write fail after the hooks.
const LEDGER: &str = r#"
name = "ledger"
label = "Ledger"
[[fields]]
name = "title"
type = "data"
required = true
unique = true
"#;

/// Enqueues `app.audit` from every hook point, with the operation in the payload; the title
/// `reject` makes a later hook set refuse.
struct Enqueuer {
    job: &'static str,
}

impl Enqueuer {
    fn enqueue(&self, ctx: &HookContext<'_>) -> Result<(), HookError> {
        ctx.enqueue(
            self.job,
            json!({"operation": format!("{:?}", ctx.operation)}),
        )
    }
}

impl DocTypeHooks for Enqueuer {
    fn before_insert(&self, ctx: &HookContext<'_>, _: &mut Document) -> Result<(), HookError> {
        self.enqueue(ctx)
    }
    fn validate(&self, ctx: &HookContext<'_>, _: &mut Document) -> Result<(), HookError> {
        // Runs for insert, update, submit and amend; the insert and amend ones are already
        // covered by `before_insert`, so only the others enqueue here.
        if matches!(
            ctx.operation,
            forgebase_core::hooks::Operation::Update | forgebase_core::hooks::Operation::Submit
        ) {
            self.enqueue(ctx)?;
        }
        Ok(())
    }
    fn before_delete(&self, ctx: &HookContext<'_>, _: &Document) -> Result<(), HookError> {
        self.enqueue(ctx)
    }
    fn before_cancel(&self, ctx: &HookContext<'_>, _: &Document) -> Result<(), HookError> {
        self.enqueue(ctx)
    }
}

/// Rejects a document whose title is `reject`.
struct Rejecter;

impl DocTypeHooks for Rejecter {
    fn validate(&self, _: &HookContext<'_>, doc: &mut Document) -> Result<(), HookError> {
        if doc.values.get("title") == Some(&Value::Text("reject".to_owned())) {
            return Err(HookError::new("rejected"));
        }
        Ok(())
    }
}

fn registry_with(hooks: Vec<Arc<dyn DocTypeHooks>>) -> Registry {
    let mut registry = registry(&[QUOTE, LEDGER]);
    for hook in hooks {
        registry.add_hooks("quote", hook.clone()).expect("hooks");
        registry.add_hooks("ledger", hook).expect("hooks");
    }
    assert!(registry.add_job("app.audit"));
    registry
}

fn quote(title: &str) -> Document {
    Document {
        name: None,
        values: BTreeMap::from([("title".to_owned(), Value::Text(title.to_owned()))]),
        tables: BTreeMap::new(),
    }
}

fn title(title: &str) -> Changes {
    Changes {
        values: BTreeMap::from([("title".to_owned(), Value::Text(title.to_owned()))]),
        tables: BTreeMap::new(),
    }
}

async fn queued(db: &TestDb) -> Vec<String> {
    db.rows("SELECT payload->>'operation' || '/' || enqueued_by || '/' || status FROM fb_job ORDER BY id")
        .await
        .into_iter()
        .map(|mut r| r.remove(0))
        .collect()
}

async fn setup(hooks: Vec<Arc<dyn DocTypeHooks>>) -> (TestDb, Registry) {
    let db = TestDb::migrated().await;
    let registry = registry_with(hooks);
    db.sync(&registry).await.expect("sync");
    (db, registry)
}

#[tokio::test]
async fn hooks_enqueue_in_the_transaction_of_every_operation() {
    let (db, registry) = setup(vec![Arc::new(Enqueuer { job: "app.audit" })]).await;
    let mut client = db.pool.get().await.unwrap();
    let ann = Caller::system("ann");

    let stored = insert(&mut **client, &registry, "quote", quote("one"), &ann)
        .await
        .unwrap();
    let stored = update(
        &mut **client,
        &registry,
        "quote",
        &stored.name,
        1,
        &title("two"),
        &ann,
    )
    .await
    .unwrap();
    let stored = submit(&mut **client, &registry, "quote", &stored.name, 2, &ann)
        .await
        .unwrap();
    let stored = cancel(&mut **client, &registry, "quote", &stored.name, 3, &ann)
        .await
        .unwrap();
    let amended = amend(&mut **client, &registry, "quote", &stored.name, 4, &ann)
        .await
        .unwrap();
    delete(&mut **client, &registry, "quote", &amended.name, 1, &ann)
        .await
        .unwrap();

    assert_eq!(
        queued(&db).await,
        [
            "Insert/ann/queued",
            "Update/ann/queued",
            "Submit/ann/queued",
            "Cancel/ann/queued",
            "Amend/ann/queued",
            "Delete/ann/queued",
        ]
    );
}

#[tokio::test]
async fn a_later_rejecting_hook_leaves_no_job() {
    let (db, registry) = setup(vec![
        Arc::new(Enqueuer { job: "app.audit" }),
        Arc::new(Rejecter),
    ])
    .await;
    // `before_insert` of the first set enqueued; `validate` of the second refuses.
    let mut client = db.pool.get().await.unwrap();
    let error = insert(
        &mut **client,
        &registry,
        "quote",
        quote("reject"),
        &Caller::system("ann"),
    )
    .await
    .unwrap_err();
    assert!(matches!(error, DbError::Hook { .. }), "{error}");
    assert!(queued(&db).await.is_empty());
}

#[tokio::test]
async fn a_database_failure_after_the_hooks_leaves_no_job() {
    let (db, registry) = setup(vec![Arc::new(Enqueuer { job: "app.audit" })]).await;
    let mut client = db.pool.get().await.unwrap();
    let ann = Caller::system("ann");
    insert(&mut **client, &registry, "ledger", quote("same"), &ann)
        .await
        .unwrap();
    assert_eq!(queued(&db).await.len(), 1);
    // The second insert runs its hooks (they enqueue) and then violates the unique title.
    let error = insert(&mut **client, &registry, "ledger", quote("same"), &ann)
        .await
        .unwrap_err();
    assert!(matches!(error, DbError::UniqueViolation { .. }), "{error}");
    assert_eq!(queued(&db).await.len(), 1);
}

#[tokio::test]
async fn a_job_name_that_is_not_known_fails_the_operation() {
    let (db, registry) = setup(vec![Arc::new(Enqueuer { job: "app.nobody" })]).await;
    let mut client = db.pool.get().await.unwrap();
    let error = insert(
        &mut **client,
        &registry,
        "quote",
        quote("one"),
        &Caller::system("ann"),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(&error, DbError::Job(message) if message.contains("app.nobody")),
        "{error}"
    );
    assert!(queued(&db).await.is_empty());
    assert_eq!(
        db.rows("SELECT count(*)::text FROM doc_quote").await,
        [["0"]]
    );
}

#[tokio::test]
async fn an_oversize_payload_is_refused_by_the_hook_context_and_by_the_queue() {
    let big = json!({"text": "x".repeat(MAX_JOB_PAYLOAD_BYTES)});
    // The context refuses it at once, so the hook can react.
    let jobs = forgebase_core::hooks::EnqueuedJobs::new();
    let ctx =
        HookContext::new("quote", "ann", forgebase_core::hooks::Operation::Insert).with_jobs(&jobs);
    assert!(ctx.enqueue("app.audit", big.clone()).is_err());
    assert!(jobs.take().is_empty());
    // The queue refuses it too (every way of enqueueing is checked).
    let (db, registry) = setup(vec![]).await;
    let client = db.pool.get().await.unwrap();
    let error = jobs::enqueue(&**client, &registry, "app.audit", &big, "ann")
        .await
        .unwrap_err();
    assert!(matches!(error, DbError::Job(_)), "{error}");
    let nul = json!({"text": "a\u{0}b"});
    assert!(
        jobs::enqueue(&**client, &registry, "app.audit", &nul, "ann")
            .await
            .is_err()
    );
    assert!(queued(&db).await.is_empty());
    for actor in ["", "a\u{0}b"] {
        let error = jobs::enqueue(&**client, &registry, "app.audit", &json!({}), actor)
            .await
            .unwrap_err();
        assert!(matches!(error, DbError::InvalidActor), "{actor:?}: {error}");
    }
    assert!(queued(&db).await.is_empty());
}

const KNOWN: [Known<'static>; 1] = [Known {
    name: "app.audit",
    lease: Duration::from_secs(60),
}];

async fn enqueue_one(db: &TestDb, registry: &Registry) -> String {
    let client = db.pool.get().await.unwrap();
    jobs::enqueue(&**client, registry, "app.audit", &json!({"n": 1}), "ann")
        .await
        .unwrap()
        .to_string()
}

#[tokio::test]
async fn claim_takes_due_known_jobs_oldest_first_and_counts_the_attempt() {
    let (db, registry) = setup(vec![]).await;
    let client = db.pool.get().await.unwrap();
    let first = enqueue_one(&db, &registry).await;
    let second = enqueue_one(&db, &registry).await;
    // A future job and a job of a name the worker does not know are not claimed.
    db.exec(
        "INSERT INTO fb_job (id, name, payload, status, enqueued_by, run_at) VALUES \
         (gen_random_uuid(), 'app.audit', '{}', 'queued', 'x', now() + interval '1 hour'), \
         (gen_random_uuid(), 'other.job', '{}', 'queued', 'x', now() - interval '1 hour')",
    )
    .await;

    let a = jobs::claim(&**client, &KNOWN, "w1").await.unwrap().unwrap();
    assert_eq!((a.id.to_string(), a.attempt), (first.clone(), 1));
    assert_eq!(a.payload, json!({"n": 1}));
    let b = jobs::claim(&**client, &KNOWN, "w1").await.unwrap().unwrap();
    assert_eq!(b.id.to_string(), second);
    assert!(
        jobs::claim(&**client, &KNOWN, "w1")
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        db.rows(&format!(
            "SELECT status, attempts::text, locked_by, (locked_until > now() + interval '50 seconds')::text FROM fb_job WHERE id = '{first}'"
        ))
        .await,
        [["running", "1", "w1", "true"]]
    );
    assert_eq!(
        db.rows("SELECT count(*)::text FROM fb_job WHERE status = 'queued'")
            .await,
        [["2"]]
    );
}

#[tokio::test]
async fn an_expired_lease_is_claimed_again_and_a_live_one_is_not() {
    let (db, registry) = setup(vec![]).await;
    let client = db.pool.get().await.unwrap();
    let id = enqueue_one(&db, &registry).await;
    let crashed = jobs::claim(&**client, &KNOWN, "w1").await.unwrap().unwrap();
    assert!(
        jobs::claim(&**client, &KNOWN, "w2")
            .await
            .unwrap()
            .is_none()
    );
    db.exec("UPDATE fb_job SET locked_until = now() - interval '1 second'")
        .await;
    let again = jobs::claim(&**client, &KNOWN, "w2").await.unwrap().unwrap();
    assert_eq!((again.id, again.attempt), (crashed.id, 2));
    assert_eq!(
        db.rows(&format!("SELECT locked_by FROM fb_job WHERE id = '{id}'"))
            .await,
        [["w2"]]
    );
    // The first worker woke up: it holds nothing any more.
    assert!(
        !jobs::complete(&**client, crashed.id, "w1", 1)
            .await
            .unwrap()
    );
    assert!(
        !jobs::complete(&**client, crashed.id, "w2", 1)
            .await
            .unwrap()
    );
    assert!(
        jobs::complete(&**client, crashed.id, "w2", 2)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn fail_retries_with_a_delay_or_kills_and_release_gives_the_attempt_back() {
    let (db, registry) = setup(vec![]).await;
    let client = db.pool.get().await.unwrap();
    let id = enqueue_one(&db, &registry).await;
    let claimed = jobs::claim(&**client, &KNOWN, "w1").await.unwrap().unwrap();

    let long = "e".repeat(5000);
    assert!(
        jobs::fail(
            &**client,
            claimed.id,
            "w1",
            1,
            &long,
            Failure::Retry(Duration::from_secs(30))
        )
        .await
        .unwrap()
    );
    assert_eq!(
        db.rows(&format!(
            "SELECT status, attempts::text, length(last_error)::text, (locked_by IS NULL)::text, \
             (run_at > now() + interval '25 seconds' AND run_at < now() + interval '35 seconds')::text \
             FROM fb_job WHERE id = '{id}'"
        ))
        .await,
        [["queued", "1", "2000", "true", "true"]]
    );
    // A guard that does not hold changes nothing.
    assert!(
        !jobs::fail(&**client, claimed.id, "w1", 1, "x", Failure::Dead)
            .await
            .unwrap()
    );

    db.exec("UPDATE fb_job SET run_at = now()").await;
    let second = jobs::claim(&**client, &KNOWN, "w1").await.unwrap().unwrap();
    assert_eq!(second.attempt, 2);
    assert!(jobs::release(&**client, second.id, "w1", 2).await.unwrap());
    assert_eq!(
        db.rows(&format!(
            "SELECT status, attempts::text FROM fb_job WHERE id = '{id}'"
        ))
        .await,
        [["queued", "1"]]
    );
    let third = jobs::claim(&**client, &KNOWN, "w1").await.unwrap().unwrap();
    assert_eq!(third.attempt, 2);
    assert!(
        jobs::fail(&**client, third.id, "w1", 2, "boom", Failure::Dead)
            .await
            .unwrap()
    );
    assert_eq!(
        db.rows(&format!(
            "SELECT status, last_error, (finished_at IS NOT NULL)::text FROM fb_job WHERE id = '{id}'"
        ))
        .await,
        [["dead", "boom", "true"]]
    );
}

#[tokio::test]
async fn list_show_and_retry_serve_the_command_line() {
    let (db, registry) = setup(vec![]).await;
    let client = db.pool.get().await.unwrap();
    let a = enqueue_one(&db, &registry).await;
    let b = enqueue_one(&db, &registry).await;
    let claimed = jobs::claim(&**client, &KNOWN, "w1").await.unwrap().unwrap();
    jobs::fail(&**client, claimed.id, "w1", 1, "boom", Failure::Dead)
        .await
        .unwrap();

    let all = jobs::list(&**client, &JobFilter::default(), 50)
        .await
        .unwrap();
    assert_eq!(
        all.iter().map(|j| j.id.to_string()).collect::<Vec<_>>(),
        [b.clone(), a.clone()]
    );
    let dead = jobs::list(
        &**client,
        &JobFilter {
            status: Some("dead"),
            name: None,
        },
        50,
    )
    .await
    .unwrap();
    assert_eq!(dead.len(), 1);
    assert_eq!(dead[0].id, claimed.id);
    let none = jobs::list(
        &**client,
        &JobFilter {
            status: None,
            name: Some("x.y"),
        },
        50,
    )
    .await
    .unwrap();
    assert!(none.is_empty());
    assert_eq!(
        jobs::list(&**client, &JobFilter::default(), 1)
            .await
            .unwrap()
            .len(),
        1
    );

    let shown = jobs::get(&**client, &a).await.unwrap().unwrap();
    assert_eq!(
        (shown.status.as_str(), shown.enqueued_by.as_str()),
        ("dead", "ann")
    );
    assert!(jobs::get(&**client, "not-a-uuid").await.is_err());

    let queued_id = if claimed.id.to_string() == a { &b } else { &a };
    assert_eq!(
        jobs::retry_dead(&**client, queued_id).await.unwrap(),
        RetryResult::NotDead("queued".to_owned())
    );
    assert_eq!(
        jobs::retry_dead(&**client, "00000000-0000-7000-8000-000000000000")
            .await
            .unwrap(),
        RetryResult::NotFound
    );
    assert_eq!(
        jobs::retry_dead(&**client, &claimed.id.to_string())
            .await
            .unwrap(),
        RetryResult::Queued
    );
    let back = jobs::get(&**client, &claimed.id.to_string())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (
            back.status.as_str(),
            back.attempts,
            back.last_error,
            back.finished_at
        ),
        ("queued", 0, None, None)
    );
}
