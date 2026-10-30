//! The job worker: claims jobs, runs their handlers and records the outcome (ADR 0013).
//!
//! The queue storage and its SQL are in `forgebase_db::jobs`; this module runs the handlers.
//!
//! One attempt, in order:
//!
//! 1. **Claim** ([`forgebase_db::jobs::claim`]): one statement, committed at once, so the lease
//!    is held while the handler runs. Only job names this binary knows are claimed.
//! 2. **Run**: a spawned task opens a transaction on its own connection, runs the handler in it
//!    and, if the handler returned `Ok`, marks the job done **in the same transaction** with a
//!    guard (still `running`, still locked by this worker, same attempt) before it commits. When
//!    the guard finds nothing, the lease was lost to another worker; the transaction is rolled
//!    back, so the handler's effects and the completion commit together or not at all.
//! 3. **Record a failure** (an error, a timeout or a panic) in a new short transaction with the
//!    same guard: a retry after the backoff, or `dead` for a permanent error or when the attempts
//!    are used up.
//!
//! A handler that runs past its timeout is aborted, and its connection is **closed**, not
//! returned to the pool, because a query may still be running on it. A handler that panics ends
//! its task; the worker records "panicked" and goes on.
//!
//! The lease is the job's timeout plus a margin ([`LEASE_MARGIN`]). A worker that dies leaves the
//! row `running`; after the lease expires another worker claims it again (the died attempt
//! counts as an attempt).

use std::collections::BTreeMap;
use std::io::Write;
use std::sync::Arc;
use std::time::{Duration, Instant};

use forgebase_core::meta::Registry;
use forgebase_db::jobs::{self, Claimed, Failure, JobConnection, Known};
use forgebase_db::schedule::{self, Spec};
use forgebase_db::{DbError, Pool};
use tokio::sync::{mpsc, watch};

use crate::jobs::{JobContext, JobError, JobHandler, JobOptions};

/// How often an idle worker looks for jobs.
pub(crate) const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// How long running jobs may finish after the first stop signal; a job still running then is
/// aborted and released (queued again, without counting the attempt).
pub(crate) const SHUTDOWN_GRACE: Duration = Duration::from_secs(30);

/// Added to a job's timeout to make its lease, so that a job that ends exactly at its timeout
/// still holds the lease while its failure is recorded.
pub(crate) const LEASE_MARGIN: Duration = Duration::from_secs(30);

/// A job of an app: its full name, handler and options.
#[derive(Clone)]
pub(crate) struct JobDef {
    pub name: String,
    pub handler: Arc<dyn JobHandler>,
    pub options: JobOptions,
}

/// The times a worker uses. The defaults are the constants above; a test sets shorter ones.
#[derive(Debug, Clone, Copy)]
pub(crate) struct WorkerConfig {
    pub poll: Duration,
    pub grace: Duration,
    pub margin: Duration,
}

impl Default for WorkerConfig {
    fn default() -> Self {
        Self {
            poll: POLL_INTERVAL,
            grace: SHUTDOWN_GRACE,
            margin: LEASE_MARGIN,
        }
    }
}

/// What became of one claimed job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// The handler succeeded and the job is `done`.
    Done,
    /// The attempt failed and the job is queued again after a delay.
    Retried,
    /// The job is `dead`.
    Dead,
    /// The lease was lost during the attempt: nothing was changed by this worker.
    LeaseLost,
    /// The worker was stopping: the job is queued again, the attempt not counted.
    Released,
}

/// The stop signal of a worker: set by the first Ctrl-C or SIGTERM.
#[derive(Clone)]
pub(crate) struct Stop {
    receiver: watch::Receiver<bool>,
    /// Keeps the channel open for a `Stop` that never fires.
    _keep: Option<Arc<watch::Sender<bool>>>,
}

impl Stop {
    /// A stop that is never set.
    pub fn never() -> Self {
        let (sender, receiver) = watch::channel(false);
        Self {
            receiver,
            _keep: Some(Arc::new(sender)),
        }
    }

    fn is_set(&self) -> bool {
        *self.receiver.borrow()
    }

    /// Ready once the signal is set; never ready if the sender is gone without setting it.
    async fn set(&mut self) {
        if self.receiver.wait_for(|stop| *stop).await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

/// What a spawned attempt ends with.
enum Finish {
    Done,
    LeaseLost,
    Failed(JobError),
}

/// The worker: the jobs of the binary and what it needs to run them.
pub(crate) struct Worker {
    pool: Pool,
    registry: Arc<Registry>,
    jobs: BTreeMap<String, JobDef>,
    /// The schedules of the binary by full job name.
    schedules: BTreeMap<String, Spec>,
    config: WorkerConfig,
    /// When this worker last ran a tick; at most one tick per poll interval, whatever the
    /// concurrency.
    last_tick: std::sync::Mutex<Option<Instant>>,
}

impl Worker {
    pub fn new(
        pool: Pool,
        registry: Registry,
        jobs: BTreeMap<String, JobDef>,
        schedules: BTreeMap<String, Spec>,
        config: WorkerConfig,
    ) -> Self {
        Self {
            pool,
            registry: Arc::new(registry),
            jobs,
            schedules,
            config,
            last_tick: std::sync::Mutex::new(None),
        }
    }

    /// True for the first caller in each poll interval (and records it), false for the others.
    fn tick_due(&self) -> bool {
        let mut last = self
            .last_tick
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let now = Instant::now();
        if last.is_some_and(|at| now.duration_since(at) < self.config.poll) {
            return false;
        }
        *last = Some(now);
        true
    }

    /// One scheduler tick: enqueues the jobs of the due schedules, once however many workers
    /// run ([`schedule::tick`]). `at` replaces the database's `now()` (tests only).
    pub async fn tick(&self, at: Option<time::OffsetDateTime>) -> Result<usize, DbError> {
        let mut client = self.pool.get().await?;
        schedule::tick(&mut **client, &self.registry, &self.schedules, at).await
    }

    pub fn job_count(&self) -> usize {
        self.jobs.len()
    }

    /// Claims one job and runs it to its outcome. `None` when nothing is due. `worker` is the
    /// id recorded in `locked_by`.
    pub async fn run_one(&self, worker: &str, stop: &Stop) -> Result<Option<Outcome>, DbError> {
        let known: Vec<Known<'_>> = self
            .jobs
            .values()
            .map(|def| Known {
                name: &def.name,
                lease: def.options.timeout.saturating_add(self.config.margin),
            })
            .collect();
        let claimed = {
            let client = self.pool.get().await?;
            jobs::claim(&**client, &known, worker).await?
        };
        let Some(claimed) = claimed else {
            return Ok(None);
        };
        // `claim` only returns names of `known`.
        let Some(def) = self.jobs.get(&claimed.name) else {
            return Ok(None);
        };
        let attempt = claimed.attempt;
        let label = format!("job {} {}", claimed.name, claimed.id);
        if u32::try_from(attempt).unwrap_or(u32::MAX) > def.options.max_attempts {
            // A worker died or timed out on every attempt (or the limit was lowered since): do
            // not run it again.
            let message = "the attempts were used up (a worker stopped while running the job, or max_attempts was lowered)";
            return self
                .record(&claimed, worker, def, &label, JobError::permanent(message))
                .await
                .map(Some);
        }

        let mut task = tokio::spawn(attempt_job(
            self.pool.clone(),
            Arc::clone(&self.registry),
            Arc::clone(&def.handler),
            claimed.clone(),
            worker.to_owned(),
        ));
        let mut stopping = stop.clone();
        let finish = tokio::select! {
            result = tokio::time::timeout(def.options.timeout, &mut task) => match result {
                Ok(Ok(finish)) => finish,
                Ok(Err(error)) => Finish::Failed(JobError::new(panic_message(error))),
                Err(_) => match stop_task(&mut task).await {
                    // The task finished in the instant before the abort: believe its result.
                    Some(finish) => finish,
                    None => Finish::Failed(JobError::new(format!(
                        "timed out after {} s",
                        def.options.timeout.as_secs_f64()
                    ))),
                },
            },
            () = async { stopping.set().await; tokio::time::sleep(self.config.grace).await } => {
                // The grace period is over: give the job back.
                let ended = stop_task(&mut task).await;
                if matches!(ended, Some(Finish::Done)) {
                    log(&format!("{label}: the job had already completed (attempt {attempt})"));
                    return Ok(Some(Outcome::Done));
                }
                if let Some(Finish::Failed(error)) = ended {
                    // The task failed just before the abort: record it through the guarded path.
                    return self.record(&claimed, worker, def, &label, error).await.map(Some);
                }
                let client = self.pool.get().await?;
                if jobs::release(&**client, claimed.id, worker, attempt).await? {
                    log(&format!("{label} released (the worker is stopping)"));
                    return Ok(Some(Outcome::Released));
                }
                log(&format!("{label}: the lease was lost or the job had already completed"));
                return Ok(Some(Outcome::LeaseLost));
            }
        };
        match finish {
            Finish::Done => {
                log(&format!("{label} done (attempt {attempt})"));
                Ok(Some(Outcome::Done))
            }
            Finish::LeaseLost => {
                log(&format!(
                    "{label}: the lease was lost during attempt {attempt}; its effects were rolled back"
                ));
                Ok(Some(Outcome::LeaseLost))
            }
            Finish::Failed(error) => self
                .record(&claimed, worker, def, &label, error)
                .await
                .map(Some),
        }
    }

    /// Records a failed attempt: retry after the backoff, or dead.
    async fn record(
        &self,
        claimed: &Claimed,
        worker: &str,
        def: &JobDef,
        label: &str,
        error: JobError,
    ) -> Result<Outcome, DbError> {
        let attempt = claimed.attempt;
        let used = u32::try_from(attempt).unwrap_or(u32::MAX);
        let dead = error.is_permanent() || used >= def.options.max_attempts;
        let failure = if dead {
            Failure::Dead
        } else {
            Failure::Retry(def.options.backoff_after(used))
        };
        let client = self.pool.get().await?;
        let recorded = jobs::fail(
            &**client,
            claimed.id,
            worker,
            attempt,
            error.message(),
            failure,
        )
        .await?;
        if !recorded {
            log(&format!(
                "{label}: the lease was lost or the job had already completed; the failure was not recorded"
            ));
            return Ok(Outcome::LeaseLost);
        }
        match failure {
            Failure::Dead => {
                log(&format!(
                    "{label} dead after attempt {attempt}: {}",
                    error.message()
                ));
                Ok(Outcome::Dead)
            }
            Failure::Retry(after) => {
                log(&format!(
                    "{label} failed (attempt {attempt} of {}), retry in {} s: {}",
                    def.options.max_attempts,
                    after.as_secs_f64(),
                    error.message()
                ));
                Ok(Outcome::Retried)
            }
        }
    }

    /// One worker slot: claims and runs jobs until `stop` is set; sleeps `poll` when idle.
    async fn run_slot(self: Arc<Self>, worker: String, mut stop: Stop) {
        while !stop.is_set() {
            // The tick comes before the claim, so a job that is due now is claimed at once. Only
            // one slot ticks per poll interval, so ticks do not grow with the concurrency.
            if self.tick_due() {
                match self.tick(None).await {
                    Ok(0) => {}
                    Ok(n) => log(&format!("scheduled {n} job(s)")),
                    Err(error) => log(&format!("error: {}", crate::error::describe_db(&error))),
                }
            }
            match self.run_one(&worker, &stop).await {
                Ok(Some(_)) => continue,
                Ok(None) => {}
                Err(error) => log(&format!("error: {}", crate::error::describe_db(&error))),
            }
            tokio::select! {
                () = tokio::time::sleep(self.config.poll) => {}
                () = stop.set() => {}
            }
        }
    }
}

/// Aborts a task and waits until it is gone (its connection is closed by then). Returns the
/// result of the task if it had finished before the abort took effect.
async fn stop_task(task: &mut tokio::task::JoinHandle<Finish>) -> Option<Finish> {
    task.abort();
    task.await.ok()
}

fn panic_message(error: tokio::task::JoinError) -> String {
    match error.try_into_panic() {
        Ok(payload) => {
            let text = payload
                .downcast_ref::<&str>()
                .map(|s| (*s).to_owned())
                .or_else(|| payload.downcast_ref::<String>().cloned());
            match text {
                Some(text) => format!("panicked: {text}"),
                None => "panicked".to_owned(),
            }
        }
        Err(_) => "the job task was cancelled".to_owned(),
    }
}

/// The task of one attempt: the handler and the completion in one transaction.
async fn attempt_job(
    pool: Pool,
    registry: Arc<Registry>,
    handler: Arc<dyn JobHandler>,
    claimed: Claimed,
    worker: String,
) -> Finish {
    match attempt(&pool, &registry, handler.as_ref(), &claimed, &worker).await {
        Ok(finish) => finish,
        Err(error) => Finish::Failed(JobError::from_db(&error)),
    }
}

async fn attempt(
    pool: &Pool,
    registry: &Registry,
    handler: &dyn JobHandler,
    claimed: &Claimed,
    worker: &str,
) -> Result<Finish, DbError> {
    // If anything below ends this function early or the task is aborted, the guard closes the
    // connection instead of returning it to the pool.
    let mut connection = JobConnection::get(pool).await?;
    let tx = connection.client().transaction().await?;
    let mut ctx = JobContext::new(tx, registry, claimed);
    let result = handler.run(&mut ctx).await;
    let tx = ctx.into_transaction();
    let finish = match result {
        Ok(()) => {
            if jobs::complete(&tx, claimed.id, worker, claimed.attempt).await? {
                tx.commit().await?;
                Finish::Done
            } else {
                tx.rollback().await?;
                Finish::LeaseLost
            }
        }
        Err(error) => {
            tx.rollback().await?;
            Finish::Failed(error)
        }
    };
    connection.finish();
    Ok(finish)
}

/// The id of this process as a worker: host, process id and a start time, so it is unique even
/// if a process id is reused. A slot adds `/N`.
pub(crate) fn worker_id() -> String {
    let host = std::fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .map(|h| h.trim().to_owned())
        .filter(|h| !h.is_empty())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_else(|| "unknown".to_owned());
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!("{host}:{}:{nanos:x}", std::process::id())
}

/// Runs `concurrency` worker slots until the first message on `signals`, then lets running jobs
/// finish for at most the grace period (the slots handle that, see [`Worker::run_one`]). A
/// second message ends the wait at once; leases recover whatever was running.
///
/// Returns `false` if a slot task panicked (the caller exits non-zero).
pub(crate) async fn run_worker(
    worker: Arc<Worker>,
    concurrency: u32,
    id: &str,
    mut signals: mpsc::UnboundedReceiver<()>,
) -> bool {
    let (sender, receiver) = watch::channel(false);
    let stop = Stop {
        receiver,
        _keep: None,
    };
    let slots: Vec<_> = (0..concurrency)
        .map(|slot| {
            tokio::spawn(Arc::clone(&worker).run_slot(format!("{id}/{slot}"), stop.clone()))
        })
        .collect();
    log(&format!(
        "started {id}: {} job type(s), concurrency {concurrency}",
        worker.job_count()
    ));
    let finished = async {
        let mut panicked = false;
        for slot in slots {
            if let Err(error) = slot.await {
                log(&format!("a worker slot ended abnormally: {error}"));
                panicked = true;
            }
        }
        panicked
    };
    tokio::pin!(finished);
    let mut panicked = false;
    tokio::select! {
        slot_panicked = &mut finished => panicked = slot_panicked,
        Some(()) = signals.recv() => {
            let _ = sender.send(true);
            log(&format!(
                "stopping: running jobs get up to {} s (send the signal again to exit now)",
                worker.config.grace.as_secs_f64()
            ));
            tokio::select! {
                slot_panicked = &mut finished => panicked = slot_panicked,
                Some(()) = signals.recv() => {
                    log("exiting now (second signal); running jobs are recovered by their lease");
                }
            }
        }
    }
    log("stopped");
    !panicked
}

/// One plain line on standard error, like the other commands of the binary.
pub(crate) fn log(message: &str) {
    let _ = writeln!(std::io::stderr(), "worker: {message}");
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use forgebase_core::document::Document;
    use forgebase_core::value::Value;

    use super::*;
    use crate::app::{App, DocTypeFile};
    use crate::builder::Forgebase;
    use crate::compose::compose;
    use crate::jobs::JobFuture;
    use crate::schedule::Schedule;
    use crate::testing::TestDb;

    const SEEN: &str = "name = \"seen\"\nlabel = \"Seen\"\n[[fields]]\nname = \"job_id\"\ntype = \"data\"\nrequired = true\nunique = true\n";

    /// One handler for all tests; the payload's `kind` says what it does.
    #[derive(Default)]
    struct Lab {
        running: Mutex<HashSet<String>>,
        clashes: AtomicUsize,
        runs: AtomicUsize,
    }

    struct LabJob(Arc<Lab>);

    impl JobHandler for LabJob {
        fn run<'a>(&'a self, ctx: &'a mut JobContext<'_>) -> JobFuture<'a> {
            Box::pin(async move {
                let lab = &self.0;
                lab.runs.fetch_add(1, Ordering::SeqCst);
                let id = ctx.id().to_owned();
                // The same job must never run twice at once.
                if !lab.running.lock().expect("lock").insert(id.clone()) {
                    lab.clashes.fetch_add(1, Ordering::SeqCst);
                }
                let sleep = ctx.payload()["sleep_ms"].as_u64().unwrap_or(0);
                let document = Document {
                    values: [("job_id".to_owned(), Value::Text(id.clone()))].into(),
                    ..Document::default()
                };
                let inserted = ctx.documents().insert("seen", document).await;
                tokio::time::sleep(Duration::from_millis(sleep)).await;
                lab.running.lock().expect("lock").remove(&id);
                inserted.map(drop)
            })
        }
    }

    fn lab_binary(lab: &Arc<Lab>, schedule: Option<Schedule>) -> Forgebase {
        let mut app = App::new("lab")
            .doctypes(const { &[DocTypeFile::new("seen.toml", SEEN)] })
            .job_with(
                "work",
                LabJob(Arc::clone(lab)),
                JobOptions::new().timeout(Duration::from_secs(3)),
            );
        if let Some(schedule) = schedule {
            app = app.schedule("work", schedule);
        }
        Forgebase::new().app(app)
    }

    fn config(grace: Duration) -> WorkerConfig {
        WorkerConfig {
            poll: Duration::from_millis(20),
            grace,
            margin: Duration::from_secs(1),
        }
    }

    /// A worker on a migrated database. No schedule is left in the database unless the test
    /// asks for one, so a built-in job that falls due while a test runs (at the turn of the
    /// hour) cannot add a job to its counts.
    async fn setup_with(
        grace: Duration,
        schedule: Option<Schedule>,
    ) -> (TestDb, Arc<Lab>, Arc<Worker>) {
        let lab = Arc::new(Lab::default());
        let binary = lab_binary(&lab, schedule);
        let db = TestDb::new().await;
        db.migrate(&binary, false).await.expect("migrate");
        let composed = compose(&binary.apps, None).expect("composes");
        let worker = Arc::new(Worker::new(
            db.pool.clone(),
            composed.registry,
            composed.jobs,
            composed.schedules,
            config(grace),
        ));
        (db, lab, worker)
    }

    async fn setup(grace: Duration) -> (TestDb, Arc<Lab>, Arc<Worker>) {
        let (db, lab, worker) = setup_with(grace, None).await;
        db.exec("DELETE FROM fb_schedule").await;
        (db, lab, worker)
    }

    async fn enqueue(db: &TestDb, payload: &str) {
        db.exec(&format!(
            "INSERT INTO fb_job (id, name, payload, status, enqueued_by) \
             VALUES (gen_random_uuid(), 'lab.work', '{payload}', 'queued', 'test')"
        ))
        .await;
    }

    async fn one(db: &TestDb, sql: &str) -> Vec<String> {
        db.rows(sql).await.remove(0)
    }

    /// Polls (every 20 ms, for at most 10 s) until `sql` returns at least one row, and returns
    /// the first. A test waits for the real precondition with this, never with a fixed sleep.
    async fn wait_row(db: &TestDb, sql: &str) -> Vec<String> {
        let started = Instant::now();
        loop {
            if let Some(row) = db.rows(sql).await.into_iter().next() {
                return row;
            }
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "the condition did not hold within 10 s: {sql}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Waits until the job is claimed by `worker` and its handler has begun its transaction: the
    /// row is `running` and locked by `worker`, and a backend is idle in a transaction (the
    /// handler sleeps). Returns that backend's pid.
    async fn wait_handler_sleeping(db: &TestDb, worker: &str) -> String {
        wait_row(
            db,
            &format!("SELECT 1 FROM fb_job WHERE status = 'running' AND locked_by = '{worker}'"),
        )
        .await;
        let database = one(db, "SELECT current_database()").await.remove(0);
        wait_row(
            db,
            &format!(
                "SELECT pid::text FROM pg_stat_activity WHERE datname = '{database}' \
                 AND state = 'idle in transaction'"
            ),
        )
        .await
        .remove(0)
    }

    async fn seen(db: &TestDb) -> String {
        one(db, "SELECT count(*)::text FROM doc_seen")
            .await
            .remove(0)
    }

    #[tokio::test]
    async fn only_one_tick_is_due_per_poll_interval_whatever_the_slots() {
        let (_db, _lab, mut worker) = setup(Duration::from_secs(5)).await;
        Arc::get_mut(&mut worker).expect("sole owner").config.poll = Duration::from_secs(3600);
        // The first slot to ask ticks; the other slots of the interval do not.
        assert!(worker.tick_due());
        assert!(!worker.tick_due());
        assert!(!worker.tick_due());
    }

    #[tokio::test]
    async fn an_expired_lease_is_recovered_and_the_job_runs() {
        let (db, lab, worker) = setup(Duration::from_secs(5)).await;
        // A worker that crashed: running, lease in the past, one attempt used.
        db.exec(
            "INSERT INTO fb_job (id, name, payload, status, attempts, locked_by, locked_until, enqueued_by) \
             VALUES (gen_random_uuid(), 'lab.work', '{}', 'running', 1, 'dead-worker', now() - interval '1 minute', 'test')",
        )
        .await;
        let outcome = worker.run_one("w", &Stop::never()).await.expect("ok");
        assert_eq!(outcome, Some(Outcome::Done));
        assert_eq!(lab.runs.load(Ordering::SeqCst), 1);
        assert_eq!(
            one(&db, "SELECT status, attempts::text, locked_by FROM fb_job").await,
            ["done", "2", "w"]
        );
        db.cleanup().await;
    }

    #[tokio::test]
    async fn a_live_lease_is_left_alone_and_exhausted_attempts_are_not_run_again() {
        let (db, lab, worker) = setup(Duration::from_secs(5)).await;
        db.exec(
            "INSERT INTO fb_job (id, name, payload, status, attempts, locked_by, locked_until, enqueued_by) \
             VALUES (gen_random_uuid(), 'lab.work', '{}', 'running', 1, 'other', now() + interval '1 minute', 'test')",
        )
        .await;
        assert_eq!(worker.run_one("w", &Stop::never()).await.expect("ok"), None);

        // The job already used all 5 attempts (the workers died on each): it is dead, not run.
        db.exec("UPDATE fb_job SET attempts = 5, locked_until = now() - interval '1 second'")
            .await;
        let outcome = worker.run_one("w", &Stop::never()).await.expect("ok");
        assert_eq!(outcome, Some(Outcome::Dead));
        assert_eq!(lab.runs.load(Ordering::SeqCst), 0);
        assert_eq!(
            one(&db, "SELECT status, attempts::text FROM fb_job").await,
            ["dead", "6"]
        );
        db.cleanup().await;
    }

    #[tokio::test]
    async fn a_lost_lease_rolls_back_the_handler_and_marks_nothing() {
        for steal in [
            "UPDATE fb_job SET locked_by = 'thief'",
            "UPDATE fb_job SET attempts = attempts + 1",
        ] {
            let (db, _lab, worker) = setup(Duration::from_secs(5)).await;
            enqueue(&db, "{\"sleep_ms\": 2000}").await;
            let monitor = async {
                // The handler's backend is idle in transaction (it sleeps); another session
                // takes the job.
                wait_handler_sleeping(&db, "w").await;
                db.exec(steal).await;
            };
            let stop = Stop::never();
            let (outcome, ()) = tokio::join!(worker.run_one("w", &stop), monitor);
            assert_eq!(outcome.expect("ok"), Some(Outcome::LeaseLost), "{steal}");
            assert_eq!(
                seen(&db).await,
                "0",
                "the handler's insert was rolled back: {steal}"
            );
            assert_eq!(
                one(&db, "SELECT status FROM fb_job").await,
                ["running"],
                "not marked done by the old worker: {steal}"
            );
            db.cleanup().await;
        }
    }

    #[tokio::test]
    async fn a_timed_out_handler_loses_its_connection_instead_of_returning_it_to_the_pool() {
        let (db, _lab, worker) = setup(Duration::from_secs(5)).await;
        enqueue(&db, "{\"sleep_ms\": 20000}").await;
        // While the handler sleeps its connection is idle in a transaction (it has begun its
        // transaction).
        let monitor = wait_handler_sleeping(&db, "w");
        let stop = Stop::never();
        let (outcome, pid) = tokio::join!(worker.run_one("w", &stop), monitor);
        assert_eq!(outcome.expect("ok"), Some(Outcome::Retried));
        // The backend of the handler is gone: the connection was closed, not pooled.
        let started = Instant::now();
        loop {
            let alive = one(
                &db,
                &format!("SELECT count(*)::text FROM pg_stat_activity WHERE pid = {pid}"),
            )
            .await
            .remove(0);
            if alive == "0" {
                break;
            }
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "the connection is still open"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(seen(&db).await, "0");
        assert_eq!(
            one(&db, "SELECT status, attempts::text FROM fb_job").await,
            ["queued", "1"]
        );
        db.cleanup().await;
    }

    fn signals() -> (mpsc::UnboundedSender<()>, mpsc::UnboundedReceiver<()>) {
        mpsc::unbounded_channel()
    }

    #[tokio::test]
    async fn four_slots_run_200_jobs_exactly_once_each() {
        let (db, lab, worker) = setup(Duration::from_secs(5)).await;
        db.exec(
            "INSERT INTO fb_job (id, name, payload, status, enqueued_by) \
             SELECT gen_random_uuid(), 'lab.work', jsonb_build_object('sleep_ms', 5, 'n', n), 'queued', 'test' \
             FROM generate_series(1, 200) AS n",
        )
        .await;
        // Two workers with two slots each, on one pool.
        let (stop_a, signals_a) = signals();
        let (stop_b, signals_b) = signals();
        let a = tokio::spawn(run_worker(Arc::clone(&worker), 2, "a", signals_a));
        let b = tokio::spawn(run_worker(Arc::clone(&worker), 2, "b", signals_b));
        let started = Instant::now();
        loop {
            let done = one(
                &db,
                "SELECT count(*)::text FROM fb_job WHERE status = 'done'",
            )
            .await
            .remove(0);
            if done == "200" {
                break;
            }
            assert!(
                started.elapsed() < Duration::from_secs(60),
                "only {done} of 200 jobs are done"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        stop_a.send(()).expect("open");
        stop_b.send(()).expect("open");
        assert!(a.await.expect("worker a"), "no slot panicked");
        assert!(b.await.expect("worker b"), "no slot panicked");
        assert_eq!(
            lab.runs.load(Ordering::SeqCst),
            200,
            "every job ran exactly once"
        );
        assert_eq!(
            lab.clashes.load(Ordering::SeqCst),
            0,
            "no job ran twice at once"
        );
        assert_eq!(
            one(
                &db,
                "SELECT count(*)::text FROM fb_job WHERE attempts <> 1 OR status <> 'done'"
            )
            .await,
            ["0"]
        );
        assert_eq!(
            seen(&db).await,
            "200",
            "no unique violation, no lost insert"
        );
        db.cleanup().await;
    }

    #[tokio::test]
    async fn the_first_signal_lets_a_running_job_finish_within_the_grace_period() {
        let (db, _lab, worker) = setup(Duration::from_secs(5)).await;
        enqueue(&db, "{\"sleep_ms\": 1500}").await;
        let (stop, receiver) = signals();
        let running = tokio::spawn(run_worker(worker, 1, "w", receiver));
        wait_handler_sleeping(&db, "w/0").await;
        stop.send(()).expect("open");
        assert!(running.await.expect("worker"), "no slot panicked");
        assert_eq!(
            one(&db, "SELECT status, attempts::text FROM fb_job").await,
            ["done", "1"]
        );
        assert_eq!(seen(&db).await, "1");
        db.cleanup().await;
    }

    #[tokio::test]
    async fn a_job_still_running_after_the_grace_period_is_released() {
        let (db, _lab, worker) = setup(Duration::from_millis(200)).await;
        // The job's own timeout is 3 s, longer than the grace period.
        enqueue(&db, "{\"sleep_ms\": 20000}").await;
        let (stop, receiver) = signals();
        let running = tokio::spawn(run_worker(worker, 1, "w", receiver));
        wait_handler_sleeping(&db, "w/0").await;
        stop.send(()).expect("open");
        let started = Instant::now();
        assert!(running.await.expect("worker"), "no slot panicked");
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(
            one(&db, "SELECT status, attempts::text, (locked_by IS NULL)::text, (locked_until IS NULL)::text FROM fb_job").await,
            ["queued", "0", "true", "true"]
        );
        assert_eq!(seen(&db).await, "0", "the aborted attempt was rolled back");
        db.cleanup().await;
    }

    #[tokio::test]
    async fn a_second_signal_exits_at_once_and_the_lease_covers_the_job() {
        let (db, _lab, worker) = setup(Duration::from_secs(60)).await;
        enqueue(&db, "{\"sleep_ms\": 20000}").await;
        let (stop, receiver) = signals();
        let running = tokio::spawn(run_worker(worker, 1, "w", receiver));
        wait_handler_sleeping(&db, "w/0").await;
        stop.send(()).expect("open");
        stop.send(()).expect("open");
        let started = Instant::now();
        assert!(running.await.expect("worker"), "no slot panicked");
        assert!(started.elapsed() < Duration::from_secs(5));
        // Nobody released it: it is still running, with a lease that will expire.
        assert_eq!(one(&db, "SELECT status FROM fb_job").await, ["running"]);
        db.cleanup().await;
    }

    #[tokio::test]
    async fn a_worker_enqueues_a_due_schedule_once_and_runs_it() {
        let (db, lab, worker) =
            setup_with(Duration::from_secs(5), Some(Schedule::every_minutes(1))).await;
        // Only the app's schedule is left, and it is due (set in the past by SQL, so the test
        // does not wait for a clock).
        db.exec("DELETE FROM fb_schedule WHERE name <> 'lab.work'")
            .await;
        db.exec(
            "UPDATE fb_schedule SET next_run_at = date_trunc('minute', now()) - interval '3 minutes'",
        )
        .await;
        let due = one(
            &db,
            "SELECT to_char(next_run_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') FROM fb_schedule",
        )
        .await
        .remove(0);
        let (stop, receiver) = signals();
        let running = tokio::spawn(run_worker(worker, 2, "w", receiver));
        // The precondition is the job being done, not elapsed time.
        wait_row(
            &db,
            &format!(
                "SELECT 1 FROM fb_job WHERE name = 'lab.work' AND status = 'done' \
                 AND payload->>'scheduled_for' = '{due}' AND enqueued_by = 'scheduler'"
            ),
        )
        .await;
        stop.send(()).expect("open");
        assert!(running.await.expect("worker"), "no slot panicked");
        // Two slots ticked, and the due time was enqueued once. (The next tick, a minute
        // boundary later, may or may not have come in the meantime: it has another due time.)
        assert_eq!(
            one(
                &db,
                &format!(
                    "SELECT count(*)::text FROM fb_job WHERE payload->>'scheduled_for' = '{due}'"
                )
            )
            .await,
            ["1"]
        );
        assert!(lab.runs.load(Ordering::SeqCst) >= 1);
        assert_eq!(
            one(
                &db,
                "SELECT (last_run_at IS NOT NULL)::text FROM fb_schedule"
            )
            .await,
            ["true"]
        );
        db.cleanup().await;
    }
}
