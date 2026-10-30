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
use std::time::Duration;

use forgebase_core::meta::Registry;
use forgebase_db::jobs::{self, Claimed, Failure, JobConnection, Known};
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
    config: WorkerConfig,
}

impl Worker {
    pub fn new(
        pool: Pool,
        registry: Registry,
        jobs: BTreeMap<String, JobDef>,
        config: WorkerConfig,
    ) -> Self {
        Self {
            pool,
            registry: Arc::new(registry),
            jobs,
            config,
        }
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
                lease: def.options.timeout + self.config.margin,
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
            // A worker died or timed out on every attempt: do not run it again.
            let message = "the attempts were used up (a worker stopped while running the job)";
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
                Err(_) => {
                    stop_task(&mut task).await;
                    Finish::Failed(JobError::new(format!(
                        "timed out after {} s",
                        def.options.timeout.as_secs_f64()
                    )))
                }
            },
            () = async { stopping.set().await; tokio::time::sleep(self.config.grace).await } => {
                // The grace period is over: give the job back.
                stop_task(&mut task).await;
                let client = self.pool.get().await?;
                if jobs::release(&**client, claimed.id, worker, attempt).await? {
                    log(&format!("{label} released (the worker is stopping)"));
                    return Ok(Some(Outcome::Released));
                }
                log(&format!("{label}: the lease was lost, nothing to release"));
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
                "{label}: the lease was lost, the failure was not recorded"
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
                    after.as_secs(),
                    error.message()
                ));
                Ok(Outcome::Retried)
            }
        }
    }

    /// One worker slot: claims and runs jobs until `stop` is set; sleeps `poll` when idle.
    async fn run_slot(self: Arc<Self>, worker: String, mut stop: Stop) {
        while !stop.is_set() {
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

/// Aborts a task and waits until it is gone (its connection is closed by then).
async fn stop_task(task: &mut tokio::task::JoinHandle<Finish>) {
    task.abort();
    let _ = task.await;
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
pub(crate) async fn run_worker(
    worker: Arc<Worker>,
    concurrency: u32,
    id: &str,
    mut signals: mpsc::UnboundedReceiver<()>,
) {
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
        for slot in slots {
            let _ = slot.await;
        }
    };
    tokio::pin!(finished);
    tokio::select! {
        () = &mut finished => {}
        Some(()) = signals.recv() => {
            let _ = sender.send(true);
            log(&format!(
                "stopping: running jobs get up to {} s (send the signal again to exit now)",
                worker.config.grace.as_secs()
            ));
            tokio::select! {
                () = &mut finished => {}
                Some(()) = signals.recv() => {
                    log("exiting now (second signal); running jobs are recovered by their lease");
                }
            }
        }
    }
    log("stopped");
}

/// One plain line on standard error, like the other commands of the binary.
pub(crate) fn log(message: &str) {
    let _ = writeln!(std::io::stderr(), "worker: {message}");
}
